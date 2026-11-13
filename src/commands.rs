use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::cli::{BaselineAction, HookAction, I18nAction, ThemeAction};
use cargo_warden::baseline::BaselineFile;
use cargo_warden::checks;
use cargo_warden::config::{LineCountMode, WardenConfig};
use cargo_warden::report::{OutputFormat, Severity};
use cargo_warden::{gate, hook};

pub(crate) struct RuntimeContext {
    pub(crate) format: OutputFormat,
    pub(crate) changeset: Option<BTreeSet<PathBuf>>,
    pub(crate) baseline_override: Option<PathBuf>,
    pub(crate) no_baseline: bool,
}

impl RuntimeContext {
    pub(crate) fn static_opts(&self, parallel: bool) -> checks::StaticRunOptions<'_> {
        checks::StaticRunOptions {
            changeset: self.changeset.as_ref(),
            baseline_override: self.baseline_override.as_deref(),
            no_baseline: self.no_baseline,
            format: self.format,
            parallel,
        }
    }
}

pub(crate) fn resolve_parallel_override(parallel: bool, no_parallel: bool) -> Option<bool> {
    if parallel {
        Some(true)
    } else if no_parallel {
        Some(false)
    } else {
        None
    }
}

fn join_paths(base_dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|p| base_dir.join(p)).collect()
}

fn resolve_baseline_target(
    config: &WardenConfig,
    base_dir: &Path,
    path_arg: Option<PathBuf>,
    ctx: &RuntimeContext,
) -> PathBuf {
    path_arg
        .map(|p| if p.is_absolute() { p } else { base_dir.join(p) })
        .or_else(|| {
            checks::resolve_baseline_path(base_dir, config, ctx.baseline_override.as_deref(), false)
        })
        .unwrap_or_else(|| base_dir.join("warden-baseline.json"))
}

pub(crate) fn handle_baseline(
    config: &WardenConfig,
    base_dir: &Path,
    action: BaselineAction,
    ctx: &RuntimeContext,
) -> Result<()> {
    match action {
        BaselineAction::Generate { out } => {
            let target = resolve_baseline_target(config, base_dir, out, ctx);
            let diags = checks::collect_all_static_checks(
                config,
                base_dir,
                &checks::StaticRunOptions {
                    changeset: None,
                    baseline_override: None,
                    no_baseline: true,
                    format: OutputFormat::Sarif,
                    parallel: true,
                },
            )?;
            let baseline = BaselineFile::from_diagnostics(&diags);
            baseline.save(&target)?;
            println!(
                "✅ [baseline] captured {} violation(s) -> {}",
                baseline.entries.len(),
                target.display()
            );
            Ok(())
        }
        BaselineAction::Check { path } => {
            let override_path = path
                .as_deref()
                .or(ctx.baseline_override.as_deref())
                .or(config.baseline.as_deref())
                .unwrap_or_else(|| Path::new("warden-baseline.json"));
            let check_ctx = RuntimeContext {
                format: ctx.format,
                changeset: ctx.changeset.clone(),
                baseline_override: Some(override_path.to_path_buf()),
                no_baseline: false,
            };
            let static_parallel = config.check.as_ref().is_none_or(|c| c.parallel);
            checks::run_static_checks_with_options(
                config,
                base_dir,
                &check_ctx.static_opts(static_parallel),
            )
        }
        BaselineAction::Prune { path, dry_run } => {
            handle_baseline_prune(config, base_dir, path, dry_run, ctx)
        }
    }
}

fn handle_baseline_prune(
    config: &WardenConfig,
    base_dir: &Path,
    path: Option<PathBuf>,
    dry_run: bool,
    ctx: &RuntimeContext,
) -> Result<()> {
    let target = resolve_baseline_target(config, base_dir, path, ctx);
    if !target.is_file() {
        bail!("Baseline file not found: {}", target.display());
    }
    let mut baseline = BaselineFile::load(&target)?;
    let diags = checks::collect_all_static_checks(
        config,
        base_dir,
        &checks::StaticRunOptions {
            changeset: None,
            baseline_override: None,
            no_baseline: true,
            format: OutputFormat::Sarif,
            parallel: true,
        },
    )?;
    let stats = baseline.prune(&diags);
    if !dry_run && stats.removed > 0 {
        baseline.save(&target)?;
    }

    if ctx.format == OutputFormat::Json {
        let payload = serde_json::json!({
            "path": target.display().to_string(),
            "dry_run": dry_run,
            "total_before": stats.total_before,
            "kept": stats.kept,
            "removed": stats.removed,
            "removed_keys": stats.removed_keys,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if stats.removed == 0 {
        println!(
            "✨ [baseline] no stale entries in {} ({} kept)",
            target.display(),
            stats.kept
        );
    } else if dry_run {
        println!(
            "🔍 [baseline] dry-run: {} stale entry(ies) would be removed from {} ({} kept):",
            stats.removed,
            target.display(),
            stats.kept
        );
        for fp in &stats.removed_keys {
            println!("  - {fp}");
        }
    } else {
        println!(
            "🧹 [baseline] pruned {} stale entry(ies) from {} ({} kept)",
            stats.removed,
            target.display(),
            stats.kept
        );
    }
    Ok(())
}

pub(crate) fn finish_single_check(
    mut diagnostics: Vec<cargo_warden::report::Diagnostic>,
    config: &WardenConfig,
    base_dir: &Path,
    ctx: &RuntimeContext,
    failure_msg: &str,
) -> Result<()> {
    if config
        .check
        .as_ref()
        .is_some_and(|c| !c.warn_unused_suppressions)
    {
        diagnostics.retain(|d| d.rule_id != "warden/unused-suppression");
    }

    let (report, stats) = checks::apply_baseline_and_report(
        diagnostics,
        base_dir,
        config,
        ctx.baseline_override.as_deref(),
        ctx.no_baseline,
    )?;
    if !ctx.format.is_structured() {
        let inline_suppressed = report
            .diagnostics
            .iter()
            .filter(|d| d.suppressed && d.suppression_reason.as_deref() == Some("inline"))
            .count();
        if inline_suppressed > 0 || stats.suppressed > 0 {
            println!(
                "ℹ️  suppressed {} finding(s) (inline: {}, baseline: {})",
                inline_suppressed + stats.suppressed,
                inline_suppressed,
                stats.suppressed
            );
        }
        for w in report
            .diagnostics
            .iter()
            .filter(|d| !d.suppressed && d.severity == Severity::Warning)
        {
            println!("  ⚠️  {}", w.message);
        }
        if report.passed {
            return Ok(());
        }
        for d in report
            .diagnostics
            .iter()
            .filter(|d| !d.suppressed && d.severity == Severity::Error)
        {
            println!("  ❌ {}", d.message);
        }
        bail!("{failure_msg}");
    }
    checks::emit_report_and_enforce(&report, stats, ctx.format, failure_msg)
}

pub(crate) fn handle_dag(
    config: WardenConfig,
    base_dir: &Path,
    root: Option<PathBuf>,
    forbid: Vec<String>,
    workspace: bool,
    ctx: &RuntimeContext,
) -> Result<()> {
    let dag_cfg = config.dag.clone().unwrap_or_default();
    let run_roots =
        root.is_some() || !workspace || config.dag.as_ref().is_some_and(|d| !d.roots.is_empty());
    let roots = if run_roots {
        root.map_or_else(|| dag_cfg.roots.clone(), |r| vec![r])
    } else {
        Vec::new()
    };
    let forbidden = if forbid.is_empty() {
        dag_cfg.forbid_imports.clone()
    } else {
        forbid
    };

    let emit_text = !ctx.format.is_structured();
    let mut diagnostics = Vec::new();
    for r in &roots {
        let diags = checks::dag::collect_dag(
            &base_dir.join(r),
            &checks::dag::DagOptions {
                base_dir,
                forbid_imports: &forbidden,
                layers: &dag_cfg.layers,
                rules: &dag_cfg.rules,
                changeset: ctx.changeset.as_ref(),
                emit_text,
            },
        )?;
        diagnostics.extend(diags);
    }

    let ws_cfg = dag_cfg.workspace.clone().unwrap_or_default();
    let run_workspace = workspace || dag_cfg.workspace.as_ref().is_some_and(|w| w.is_active());
    if run_workspace {
        let ws_diags = checks::dag::workspace::collect_workspace_dag(
            &checks::dag::workspace::WorkspaceDagOptions {
                base_dir,
                config: &ws_cfg,
                changeset: ctx.changeset.as_ref(),
                emit_text,
            },
        )?;
        diagnostics.extend(ws_diags);
    }

    if emit_text
        && !diagnostics
            .iter()
            .any(|d| !d.suppressed && d.severity == Severity::Error)
    {
        if !roots.is_empty() {
            println!("[dag] OK: dependencies form a clean DAG with 0 cycle(s)");
        }
        if run_workspace {
            println!("[workspace-dag] OK: workspace crate dependencies satisfy all boundary rules");
        }
    }

    let failure_msg = if diagnostics
        .iter()
        .any(|d| !d.suppressed && d.rule_id == "warden/dag/cycle")
    {
        "Module dependency cycle(s) detected"
    } else if diagnostics
        .iter()
        .any(|d| !d.suppressed && d.rule_id == "warden/dag/forbid-import")
    {
        "Forbidden imports detected (modules depending on assembly/forbidden targets)"
    } else if diagnostics
        .iter()
        .any(|d| !d.suppressed && d.rule_id.starts_with("warden/dag/workspace-"))
    {
        "Workspace crate dependency boundary violation(s) detected"
    } else {
        "Layered architecture boundary violation(s) detected"
    };

    finish_single_check(diagnostics, &config, base_dir, ctx, failure_msg)
}

pub(crate) fn handle_ci_parity(
    config: WardenConfig,
    base_dir: &Path,
    workflow: Option<PathBuf>,
    ignore: Vec<String>,
    ctx: &RuntimeContext,
) -> Result<()> {
    let ci_cfg = config.ci_parity.clone().unwrap_or_default();
    let wf = workflow.unwrap_or_else(|| ci_cfg.workflow.clone());
    let ignored = if ignore.is_empty() {
        ci_cfg.ignore_crates.clone()
    } else {
        ignore
    };

    let diagnostics = checks::ci_parity::collect_ci_parity(
        base_dir,
        &checks::ci_parity::CiParityOptions {
            ci_file: &wf,
            test_patterns: &ci_cfg.test_patterns,
            ignore_crates: &ignored,
            fanout_expected: &ci_cfg.fanout_crates,
            gate_cfg: config.gate.as_ref(),
            changeset: ctx.changeset.as_ref(),
            emit_text: !ctx.format.is_structured(),
        },
    )?;

    finish_single_check(
        diagnostics,
        &config,
        base_dir,
        ctx,
        "Workspace members missing from CI parity",
    )
}

pub(crate) fn handle_theme(
    config: WardenConfig,
    base_dir: &Path,
    action: ThemeAction,
    ctx: &RuntimeContext,
) -> Result<()> {
    let mut theme_cfg = config.slint_theme.clone().unwrap_or_default();
    match action {
        ThemeAction::Check {
            ui_dir,
            spacing,
            radius,
            typography,
            duration,
            all_rules,
            unknown_tokens,
            unused_tokens,
            verify_generated,
        } => {
            if all_rules {
                theme_cfg.rules.color = true;
                theme_cfg.rules.spacing = true;
                theme_cfg.rules.radius = true;
                theme_cfg.rules.typography = true;
                theme_cfg.rules.duration = true;
            } else {
                theme_cfg.rules.spacing |= spacing;
                theme_cfg.rules.radius |= radius;
                theme_cfg.rules.typography |= typography;
                theme_cfg.rules.duration |= duration;
            }
            theme_cfg.check_unknown_tokens |= unknown_tokens;
            theme_cfg.check_unused_tokens |= unused_tokens;
            theme_cfg.verify_generated |= verify_generated;

            let dir = ui_dir.unwrap_or_else(|| theme_cfg.ui_dir.clone());
            let emit_text = !ctx.format.is_structured();
            let full_dir = base_dir.join(&dir);
            let diagnostics = checks::slint_theme::collect_theme_with_policy(
                &full_dir,
                &checks::slint_theme::SlintThemePolicy {
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
                    changeset: ctx.changeset.as_ref(),
                    emit_text,
                    parallel: true,
                },
            )?;
            if emit_text
                && !diagnostics
                    .iter()
                    .any(|d| !d.suppressed && d.severity == Severity::Error)
            {
                println!(
                    "[slint-theme] OK: {} satisfies all enabled Slint token/UI rules",
                    full_dir.display()
                );
            }
            finish_single_check(
                diagnostics,
                &config,
                base_dir,
                ctx,
                "Slint design token / UI specification violation(s) remain",
            )
        }
        ThemeAction::Apply { ui_dir } => {
            let dir = ui_dir.unwrap_or(theme_cfg.ui_dir);
            let mapping =
                checks::slint_theme::load_token_mapping(base_dir, theme_cfg.theme_def.as_deref());
            checks::slint_theme::apply_tokens_with_out_file(
                &base_dir.join(dir),
                &base_dir.join(&theme_cfg.out_file),
                &mapping,
                &theme_cfg.unchanged_colors,
            )
        }
        ThemeAction::Generate { out_file } => {
            let out = out_file.unwrap_or(theme_cfg.out_file);
            let def = match &theme_cfg.theme_def {
                Some(def_path) => {
                    checks::slint_theme::load_theme_definition(&base_dir.join(def_path))?
                }
                None => checks::slint_theme::ThemeDefinition::default(),
            };
            checks::slint_theme::generate_theme(&def, &base_dir.join(out))
        }
    }
}

pub(crate) fn handle_i18n(
    config: WardenConfig,
    base_dir: &Path,
    action: I18nAction,
    ctx: &RuntimeContext,
) -> Result<()> {
    let i18n_cfg = config.i18n.clone().unwrap_or_default();
    match action {
        I18nAction::Check {
            lang_dir,
            out_file,
            source_dirs,
            strict,
            check_unused,
            no_placeholders,
        } => {
            let overrides = checks::I18nOverrides {
                lang_dir,
                out_file,
                source_dirs: &source_dirs,
                strict,
                check_unused,
                no_placeholders,
            };
            let diagnostics = checks::collect_i18n_from_config(
                &i18n_cfg,
                base_dir,
                &overrides,
                ctx.changeset.as_ref(),
                !ctx.format.is_structured(),
            )?;
            finish_single_check(
                diagnostics,
                &config,
                base_dir,
                ctx,
                "Translation catalog validation failed",
            )
        }
        I18nAction::Generate { lang_dir, out_file } => {
            let ldir = lang_dir.unwrap_or(i18n_cfg.lang_dir);
            let ofile = out_file.unwrap_or(i18n_cfg.out_file);
            checks::i18n_po::sync_i18n(
                &base_dir.join(ldir),
                &i18n_cfg.domain,
                &i18n_cfg.locales,
                &base_dir.join(ofile),
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_complexity(
    config: WardenConfig,
    base_dir: &Path,
    max: Option<usize>,
    max_lines: Option<usize>,
    max_nesting: Option<usize>,
    paths: Vec<PathBuf>,
    no_ignore_tests: bool,
    no_gitignore: bool,
    ctx: &RuntimeContext,
) -> Result<()> {
    let comp_cfg = config.complexity.clone().unwrap_or_default();
    let max_score = max.unwrap_or(comp_cfg.max);
    let raw_paths = if paths.is_empty() {
        &comp_cfg.paths
    } else {
        &paths
    };
    let search_paths = join_paths(base_dir, raw_paths);
    let ignore_tests = !no_ignore_tests && comp_cfg.ignore_tests;
    let respect_gitignore = !no_gitignore && comp_cfg.respect_gitignore;
    let emit_text = !ctx.format.is_structured();

    let policy = checks::complexity::ComplexityPolicy {
        max_function_lines: max_lines.or(comp_cfg.max_function_lines),
        max_nesting: max_nesting.or(comp_cfg.max_nesting),
        overrides: &comp_cfg.overrides,
    };

    let (reports, files_scanned, diagnostics) = checks::complexity::collect_complexity_with_policy(
        &search_paths,
        &checks::complexity::ComplexityOptions {
            base_dir,
            max_complexity: max_score,
            ignore_tests,
            ignore_patterns: &comp_cfg.ignore,
            respect_gitignore,
            changeset: ctx.changeset.as_ref(),
            emit_text,
        },
        &policy,
    )?;

    if emit_text
        && !diagnostics
            .iter()
            .any(|d| !d.suppressed && d.severity == Severity::Error)
    {
        println!(
            "[complexity] OK: scanned {} functions across {} files, all within limit ({})",
            reports.len(),
            files_scanned,
            max_score
        );
    }

    finish_single_check(
        diagnostics,
        &config,
        base_dir,
        ctx,
        "Cognitive complexity threshold exceeded",
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_file_length(
    config: WardenConfig,
    base_dir: &Path,
    max_lines: Option<usize>,
    ext: Vec<String>,
    paths: Vec<PathBuf>,
    effective: bool,
    no_gitignore: bool,
    ctx: &RuntimeContext,
) -> Result<()> {
    let fl_cfg = config.file_length.clone().unwrap_or_default();
    let limit = max_lines.unwrap_or(fl_cfg.max_lines);
    let extensions = if ext.is_empty() {
        fl_cfg.extensions.clone()
    } else {
        ext
    };
    let raw_paths = if paths.is_empty() {
        &fl_cfg.paths
    } else {
        &paths
    };
    let search_paths = join_paths(base_dir, raw_paths);
    let mode = if effective {
        LineCountMode::Effective
    } else {
        fl_cfg.mode
    };
    let respect_gitignore = !no_gitignore && fl_cfg.respect_gitignore;
    let emit_text = !ctx.format.is_structured();

    let (reports, diagnostics) = checks::file_length::collect_file_length_with_overrides(
        &search_paths,
        &checks::file_length::FileLengthOptions {
            base_dir,
            max_lines: limit,
            extensions: &extensions,
            ignore_patterns: &fl_cfg.ignore,
            mode,
            respect_gitignore,
            changeset: ctx.changeset.as_ref(),
            emit_text,
        },
        &fl_cfg.overrides,
        true,
    )?;

    if emit_text
        && !diagnostics
            .iter()
            .any(|d| !d.suppressed && d.severity == Severity::Error)
    {
        let mode_label = match mode {
            LineCountMode::Physical => "lines",
            LineCountMode::Effective => "effective lines",
        };
        println!(
            "[file-length] OK: scanned {} file(s), all within {} {}",
            reports.len(),
            limit,
            mode_label
        );
    }

    finish_single_check(
        diagnostics,
        &config,
        base_dir,
        ctx,
        "Maximum file line count exceeded",
    )
}

pub(crate) fn handle_check(
    config: &WardenConfig,
    base_dir: &Path,
    full: bool,
    fix: bool,
    parallel: bool,
    no_parallel: bool,
    ctx: &RuntimeContext,
) -> Result<()> {
    let parallel_override = resolve_parallel_override(parallel, no_parallel);
    let static_parallel =
        parallel_override.unwrap_or_else(|| config.check.as_ref().is_none_or(|c| c.parallel));

    if fix {
        checks::run_static_fixes(config, base_dir, !ctx.format.is_structured())?;
        gate::runner::run_check_toolchain_steps_with_options(
            base_dir,
            config,
            full,
            true,
            parallel_override,
        )?;
        return checks::run_static_checks_with_options(
            config,
            base_dir,
            &ctx.static_opts(static_parallel),
        );
    }

    checks::run_static_checks_with_options(config, base_dir, &ctx.static_opts(static_parallel))?;
    gate::runner::run_check_toolchain_steps_with_options(
        base_dir,
        config,
        full,
        false,
        parallel_override,
    )
}

pub(crate) fn handle_hook(action: HookAction, base_dir: &Path) -> Result<()> {
    match action {
        HookAction::Install {
            hook: kind,
            staged,
            gate,
            full,
            force,
            append,
        } => handle_hook_install(base_dir, kind.into(), staged, gate, full, force, append),
        HookAction::Uninstall { hook: kind } => {
            match hook::uninstall_hook(base_dir, kind.into())? {
                hook::UninstallOutcome::RemovedFile(path) => {
                    println!("🗑️  [hook] removed {}", path.display());
                    Ok(())
                }
                hook::UninstallOutcome::RemovedBlock(path) => {
                    println!(
                        "✂️  [hook] removed cargo-warden block from {}",
                        path.display()
                    );
                    Ok(())
                }
                hook::UninstallOutcome::NotInstalled(path) => {
                    println!("ℹ️  [hook] {} is not installed", path.display());
                    Ok(())
                }
            }
        }
        HookAction::Status => hook::print_hook_status(base_dir),
    }
}

fn handle_hook_install(
    base_dir: &Path,
    kind: hook::HookKind,
    staged: bool,
    gate: bool,
    full: bool,
    force: bool,
    append: bool,
) -> Result<()> {
    let mode = if full {
        hook::HookMode::GateFull
    } else if gate {
        hook::HookMode::Gate
    } else if staged {
        hook::HookMode::CheckStaged
    } else {
        hook::HookMode::Check
    };
    let policy = if force {
        hook::ExistingHookPolicy::Force
    } else if append {
        hook::ExistingHookPolicy::Append
    } else {
        hook::ExistingHookPolicy::Fail
    };

    let (label, path) = match hook::install_hook(base_dir, kind, mode, policy)? {
        hook::InstallOutcome::Created(p) => ("✅ [hook] installed", p),
        hook::InstallOutcome::Updated(p) => ("🔄 [hook] updated", p),
        hook::InstallOutcome::Appended(p) => ("➕ [hook] appended", p),
        hook::InstallOutcome::Overwritten(p) => ("⚠️  [hook] overwrote", p),
    };
    println!(
        "{label} {} ({}) at {}",
        kind.file_name(),
        mode.label(),
        path.display()
    );
    Ok(())
}

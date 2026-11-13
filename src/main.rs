mod cli;

use anyhow::Result;
use clap::Parser;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cargo_warden::baseline::BaselineFile;
use cargo_warden::config::{LineCountMode, WardenConfig};
use cargo_warden::report::OutputFormat;
use cargo_warden::{checks, gate, git, hook, init};
use cli::{BaselineAction, Cli, Commands, HookAction, I18nAction, ThemeAction};

struct RuntimeContext {
    format: OutputFormat,
    changeset: Option<BTreeSet<PathBuf>>,
    baseline_override: Option<PathBuf>,
    no_baseline: bool,
}

impl RuntimeContext {
    fn static_opts(&self, parallel: bool) -> checks::StaticRunOptions<'_> {
        checks::StaticRunOptions {
            changeset: self.changeset.as_ref(),
            baseline_override: self.baseline_override.as_deref(),
            no_baseline: self.no_baseline,
            format: self.format,
            parallel,
        }
    }
}

fn resolve_parallel_override(parallel: bool, no_parallel: bool) -> Option<bool> {
    if parallel {
        Some(true)
    } else if no_parallel {
        Some(false)
    } else {
        None
    }
}

fn init_tracing(verbose: u8, quiet: bool) {
    let default_directive = if quiet {
        "cargo_warden=error"
    } else {
        match verbose {
            0 => "cargo_warden=warn",
            1 => "cargo_warden=info",
            2 => "cargo_warden=debug",
            _ => "cargo_warden=trace",
        }
    };

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_directive));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .without_time();

    if verbose >= 2 {
        builder.with_target(true).init();
    } else {
        builder.with_target(false).init();
    }
}

fn join_paths(base_dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|p| base_dir.join(p)).collect()
}

fn load_config(cfg_path: Option<&PathBuf>, base_dir: &Path) -> Result<WardenConfig> {
    if let Some(path) = cfg_path {
        tracing::debug!(config_path = %path.display(), "loading explicit configuration file");
        let content = std::fs::read_to_string(path)?;
        return Ok(toml::from_str(&content)?);
    }
    tracing::debug!("auto-loading warden configuration");
    WardenConfig::load(base_dir)
}

fn main() -> Result<()> {
    let mut raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() > 1 && raw_args[1] == "warden" {
        raw_args.remove(1);
    }

    let cli = Cli::parse_from(raw_args);

    init_tracing(cli.verbose, cli.quiet);
    tracing::debug!(
        verbose = cli.verbose,
        quiet = cli.quiet,
        "tracing initialized"
    );

    let base_dir = match &cli.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir()?,
    };
    tracing::debug!(base_dir = %base_dir.display(), "base directory resolved");

    let scope = if cli.staged {
        git::ChangesetScope::Staged
    } else if let Some(base) = cli.diff {
        git::ChangesetScope::Diff(base)
    } else {
        git::ChangesetScope::All
    };
    let changeset = git::resolve_changeset(&base_dir, &scope)?;

    let ctx = RuntimeContext {
        format: cli.format.into(),
        changeset,
        baseline_override: cli.baseline,
        no_baseline: cli.no_baseline,
    };

    if let Commands::Init {
        force,
        dotfile,
        interactive,
    } = cli.command
    {
        init::run_init(
            &base_dir,
            &init::InitOptions {
                force,
                dotfile,
                interactive,
            },
        )?;
        return Ok(());
    }

    let config = load_config(cli.config.as_ref(), &base_dir)?;
    dispatch_command(cli.command, config, &base_dir, &ctx)
}

fn dispatch_command(
    command: Commands,
    config: WardenConfig,
    base_dir: &Path,
    ctx: &RuntimeContext,
) -> Result<()> {
    match command {
        Commands::Init { .. } => unreachable!("handled prior to config loading"),
        Commands::Baseline { action } => handle_baseline(&config, base_dir, action, ctx),
        Commands::Dag { root, forbid } => handle_dag(config, base_dir, root, forbid, ctx),
        Commands::CiParity { workflow, ignore } => {
            handle_ci_parity(config, base_dir, workflow, ignore, ctx)
        }
        Commands::Theme { action } => handle_theme(config, base_dir, action, ctx),
        Commands::I18n { action } => handle_i18n(config, base_dir, action, ctx),
        Commands::Complexity {
            max,
            paths,
            no_ignore_tests,
            no_gitignore,
        } => handle_complexity(
            config,
            base_dir,
            max,
            paths,
            no_ignore_tests,
            no_gitignore,
            ctx,
        ),
        Commands::FileLength {
            max_lines,
            ext,
            paths,
            effective,
            no_gitignore,
        } => handle_file_length(
            config,
            base_dir,
            max_lines,
            ext,
            paths,
            effective,
            no_gitignore,
            ctx,
        ),
        Commands::Gate {
            full,
            keep_going,
            skip_checks,
            fix,
            parallel,
            no_parallel,
            only,
            skip,
        } => {
            let parallel_override = resolve_parallel_override(parallel, no_parallel);
            let static_parallel = parallel_override
                .unwrap_or_else(|| config.gate.as_ref().is_none_or(|g| g.parallel));
            let opts = gate::runner::GateRunOptions {
                full,
                keep_going,
                skip_checks,
                fix,
                parallel: parallel_override,
                only,
                skip,
            };
            gate::runner::run_gate_with_static_options(
                base_dir,
                &config,
                &opts,
                &ctx.static_opts(static_parallel),
            )
        }
        Commands::Check {
            full,
            fix,
            parallel,
            no_parallel,
        } => handle_check(&config, base_dir, full, fix, parallel, no_parallel, ctx),
        Commands::Hook { action } => handle_hook(action, base_dir),
    }
}

fn handle_baseline(
    config: &WardenConfig,
    base_dir: &Path,
    action: BaselineAction,
    ctx: &RuntimeContext,
) -> Result<()> {
    match action {
        BaselineAction::Generate { out } => {
            let target = out
                .map(|p| if p.is_absolute() { p } else { base_dir.join(p) })
                .or_else(|| {
                    checks::resolve_baseline_path(
                        base_dir,
                        config,
                        ctx.baseline_override.as_deref(),
                        false,
                    )
                })
                .unwrap_or_else(|| base_dir.join("warden-baseline.json"));

            let diags = checks::collect_all_static_checks(
                config,
                base_dir,
                &checks::StaticRunOptions {
                    changeset: None,
                    baseline_override: None,
                    no_baseline: true,
                    format: OutputFormat::Sarif, // suppress per-check stdout chatter
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
    }
}

fn finish_single_check(
    diagnostics: Vec<cargo_warden::report::Diagnostic>,
    config: &WardenConfig,
    base_dir: &Path,
    ctx: &RuntimeContext,
    failure_msg: &str,
) -> Result<()> {
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
        if report.passed {
            return Ok(());
        }
        for d in report.diagnostics.iter().filter(|d| !d.suppressed) {
            println!("  ❌ {}", d.message);
        }
        anyhow::bail!("{failure_msg}");
    }
    checks::emit_report_and_enforce(&report, stats, ctx.format, failure_msg)
}

fn handle_dag(
    config: WardenConfig,
    base_dir: &Path,
    root: Option<PathBuf>,
    forbid: Vec<String>,
    ctx: &RuntimeContext,
) -> Result<()> {
    let dag_cfg = config.dag.clone().unwrap_or_default();
    let roots = root.map_or_else(|| dag_cfg.roots.clone(), |r| vec![r]);
    let forbidden = if forbid.is_empty() {
        dag_cfg.forbid_imports.clone()
    } else {
        forbid
    };

    let emit_text = !ctx.format.is_structured();
    let mut diagnostics = Vec::new();
    for r in roots {
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

    if emit_text && !diagnostics.iter().any(|d| !d.suppressed) {
        println!("[dag] OK: dependencies form a clean DAG with 0 cycle(s)");
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
    } else {
        "Layered architecture boundary violation(s) detected"
    };

    finish_single_check(diagnostics, &config, base_dir, ctx, failure_msg)
}

fn handle_ci_parity(
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

fn handle_theme(
    config: WardenConfig,
    base_dir: &Path,
    action: ThemeAction,
    ctx: &RuntimeContext,
) -> Result<()> {
    let theme_cfg = config.slint_theme.clone().unwrap_or_default();
    match action {
        ThemeAction::Check { ui_dir } => {
            let dir = ui_dir.unwrap_or_else(|| theme_cfg.ui_dir.clone());
            let emit_text = !ctx.format.is_structured();
            let full_dir = base_dir.join(&dir);
            let diagnostics = checks::slint_theme::collect_theme(
                &full_dir,
                &checks::slint_theme::SlintThemeOptions {
                    base_dir,
                    unchanged: &theme_cfg.unchanged_colors,
                    respect_gitignore: theme_cfg.respect_gitignore,
                    changeset: ctx.changeset.as_ref(),
                    emit_text,
                },
            )?;
            if emit_text && !diagnostics.iter().any(|d| !d.suppressed) {
                println!(
                    "[slint-theme] OK: all hex colors in {} are mapped or whitelisted",
                    full_dir.display()
                );
            }
            finish_single_check(
                diagnostics,
                &config,
                base_dir,
                ctx,
                "Unmapped hex colors remain in .slint components",
            )
        }
        ThemeAction::Apply { ui_dir } => {
            let dir = ui_dir.unwrap_or(theme_cfg.ui_dir);
            let mapping =
                checks::slint_theme::load_token_mapping(base_dir, theme_cfg.theme_def.as_deref());
            checks::slint_theme::apply_tokens(
                &base_dir.join(dir),
                &mapping,
                &theme_cfg.unchanged_colors,
            )
        }
        ThemeAction::Generate { out_file } => {
            let out = out_file.unwrap_or(theme_cfg.out_file);
            let def = match &theme_cfg.theme_def {
                Some(def_path) => {
                    let content = std::fs::read_to_string(base_dir.join(def_path))?;
                    toml::from_str(&content)?
                }
                None => checks::slint_theme::ThemeDefinition::default(),
            };
            checks::slint_theme::generate_theme(&def, &base_dir.join(out))
        }
    }
}

fn handle_i18n(
    config: WardenConfig,
    base_dir: &Path,
    action: I18nAction,
    ctx: &RuntimeContext,
) -> Result<()> {
    let i18n_cfg = config.i18n.clone().unwrap_or_default();
    match action {
        I18nAction::Check { lang_dir, out_file } => {
            let ldir = lang_dir.unwrap_or_else(|| i18n_cfg.lang_dir.clone());
            let ofile = out_file.unwrap_or_else(|| i18n_cfg.out_file.clone());
            let diagnostics = checks::i18n_po::collect_i18n(&checks::i18n_po::I18nOptions {
                base_dir,
                lang_dir: &base_dir.join(ldir),
                domain: &i18n_cfg.domain,
                locales: &i18n_cfg.locales,
                out_file: &base_dir.join(ofile),
                changeset: ctx.changeset.as_ref(),
                emit_text: !ctx.format.is_structured(),
            })?;
            finish_single_check(
                diagnostics,
                &config,
                base_dir,
                ctx,
                "Translation catalog is out of sync with .po catalogs",
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

fn handle_complexity(
    config: WardenConfig,
    base_dir: &Path,
    max: Option<usize>,
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

    let (reports, files_scanned, diagnostics) = checks::complexity::collect_complexity(
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
    )?;

    if emit_text && !diagnostics.iter().any(|d| !d.suppressed) {
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
fn handle_file_length(
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

    let (reports, diagnostics) = checks::file_length::collect_file_length(
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
    )?;

    if emit_text && !diagnostics.iter().any(|d| !d.suppressed) {
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

fn handle_check(
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

fn handle_hook(action: HookAction, base_dir: &Path) -> Result<()> {
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
        HookAction::Status => handle_hook_status(base_dir),
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

fn handle_hook_status(base_dir: &Path) -> Result<()> {
    for entry in hook::inspect_hooks(base_dir)? {
        match entry.state {
            hook::HookState::NotInstalled => {
                println!(
                    "  - {}: not installed ({})",
                    entry.kind.file_name(),
                    entry.path.display()
                );
            }
            hook::HookState::Managed { mode, executable } => {
                let mode_str = mode.map_or("unknown", hook::HookMode::label);
                let exec_str = if executable {
                    "executable"
                } else {
                    "NOT executable"
                };
                println!(
                    "  ✅ {}: managed ({}, {}) [{}]",
                    entry.kind.file_name(),
                    mode_str,
                    exec_str,
                    entry.path.display()
                );
            }
            hook::HookState::Unmanaged { executable } => {
                let exec_str = if executable {
                    "executable"
                } else {
                    "NOT executable"
                };
                println!(
                    "  ⚠️  {}: custom/unmanaged ({}) [{}]",
                    entry.kind.file_name(),
                    exec_str,
                    entry.path.display()
                );
            }
        }
    }
    Ok(())
}

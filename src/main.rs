#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cli;
mod commands;
mod commands_extra;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use std::path::{Path, PathBuf};

use cargo_warden::config::WardenConfig;
use cargo_warden::{doctor, gate, git, init};
use cli::{Cli, Commands};
use commands::{
    RuntimeContext, handle_baseline, handle_check, handle_ci_parity, handle_complexity, handle_dag,
    handle_file_length, handle_hook, handle_i18n, handle_theme, resolve_parallel_override,
};
use commands_extra::{handle_actions, handle_hygiene, handle_manifest};

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

    if let Commands::Completions { shell } = cli.command {
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "cargo-warden", &mut std::io::stdout());
        return Ok(());
    }

    let base_dir = match &cli.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir()?,
    };
    tracing::debug!(base_dir = %base_dir.display(), "base directory resolved");

    if let Commands::Doctor { strict } = cli.command {
        return doctor::run_doctor(&base_dir, cli.config.as_deref(), strict, cli.format.into());
    }

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

    let scope = if cli.staged {
        git::ChangesetScope::Staged
    } else if let Some(base) = cli.diff {
        git::ChangesetScope::Diff(base)
    } else {
        git::ChangesetScope::All
    };
    let changeset = git::resolve_changeset(&base_dir, &scope)?;
    let gate_changeset = if matches!(cli.command, Commands::Gate { .. }) && scope.is_incremental() {
        git::resolve_changeset_with_deleted(&base_dir, &scope)?
    } else {
        None
    };

    let ctx = RuntimeContext {
        format: cli.format.into(),
        changeset,
        baseline_override: cli.baseline,
        no_baseline: cli.no_baseline,
    };

    let config = load_config(cli.config.as_ref(), &base_dir)?;
    dispatch_command(cli.command, config, &base_dir, &ctx, gate_changeset)
}

fn dispatch_command(
    command: Commands,
    config: WardenConfig,
    base_dir: &Path,
    ctx: &RuntimeContext,
    gate_changeset: Option<std::collections::BTreeSet<PathBuf>>,
) -> Result<()> {
    match command {
        Commands::Init { .. } | Commands::Completions { .. } | Commands::Doctor { .. } => {
            unreachable!("handled prior to config loading")
        }
        Commands::Baseline { action } => handle_baseline(&config, base_dir, action, ctx),
        Commands::Dag {
            root,
            forbid,
            workspace,
        } => handle_dag(config, base_dir, root, forbid, workspace, ctx),
        Commands::CiParity { workflow, ignore } => {
            handle_ci_parity(config, base_dir, workflow, ignore, ctx)
        }
        Commands::Actions {
            workflow,
            ignore,
            strict,
            require_sha,
            fix,
            pin_sha,
            offline,
        } => handle_actions(
            config,
            base_dir,
            workflow,
            ignore,
            strict,
            require_sha,
            fix,
            pin_sha,
            offline,
            ctx,
        ),
        Commands::Theme { action } => handle_theme(config, base_dir, action, ctx),
        Commands::I18n { action } => handle_i18n(config, base_dir, action, ctx),
        Commands::Complexity {
            max,
            max_lines,
            max_nesting,
            max_params,
            paths,
            no_ignore_tests,
            no_gitignore,
        } => handle_complexity(
            config,
            base_dir,
            max,
            max_lines,
            max_nesting,
            max_params,
            paths,
            no_ignore_tests,
            no_gitignore,
            ctx,
        ),
        Commands::Hygiene {
            paths,
            forbid_unsafe,
            forbid_expect,
            forbid_println,
            no_ignore_tests,
            no_gitignore,
        } => handle_hygiene(
            config,
            base_dir,
            paths,
            forbid_unsafe,
            forbid_expect,
            forbid_println,
            no_ignore_tests,
            no_gitignore,
            ctx,
        ),
        Commands::Manifest {
            ignore,
            check_features,
        } => handle_manifest(config, base_dir, ignore, check_features, ctx),
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
                impact_changeset: gate_changeset,
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

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

use cargo_warden::config::WardenConfig;
use cargo_warden::{checks, gate, hook};

#[derive(Parser, Debug)]
#[command(
    name = "cargo-warden",
    bin_name = "cargo warden",
    version,
    about = "Architecture boundaries, quality gates, cognitive complexity and drift prevention",
    long_about = "A unified Cargo plugin and code quality toolkit for Rust and Slint projects."
)]
struct Cli {
    /// Custom path to warden.toml configuration file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Working directory (defaults to current directory)
    #[arg(short = 'C', long, global = true)]
    cwd: Option<PathBuf>,

    /// Increase logging verbosity (-v for info, -vv for debug, -vvv for trace)
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Suppress non-essential logging output
    #[arg(short = 'q', long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Verify module dependency boundaries and detect dependency cycles
    Dag {
        /// Source directory to inspect (e.g. `src` or `crates/core/src`)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Top-level modules that must not be imported
        #[arg(long)]
        forbid: Vec<String>,
    },

    /// Ensure all workspace members are covered in CI test runs
    CiParity {
        /// Path to the CI workflow file (default: `.github/workflows/ci.yml`)
        #[arg(long)]
        workflow: Option<PathBuf>,
        /// Ignore specific workspace crates
        #[arg(long)]
        ignore: Vec<String>,
    },

    /// Slint UI theme color consistency tools
    Theme {
        #[command(subcommand)]
        action: ThemeAction,
    },

    /// Gettext .po translation catalog tools
    I18n {
        #[command(subcommand)]
        action: I18nAction,
    },

    /// Scan Rust functions for cognitive complexity violations
    Complexity {
        /// Maximum allowed cognitive complexity score per function
        #[arg(long)]
        max: Option<usize>,
        /// Paths or files to inspect
        #[arg(long)]
        paths: Vec<PathBuf>,
        /// Do not ignore test files
        #[arg(long)]
        no_ignore_tests: bool,
    },

    /// Check files against a maximum line count threshold
    FileLength {
        /// Maximum allowed line count per file
        #[arg(long)]
        max_lines: Option<usize>,
        /// File extensions to check (e.g. `rs`, `slint`)
        #[arg(long)]
        ext: Vec<String>,
        /// Paths to scan
        #[arg(long)]
        paths: Vec<PathBuf>,
    },

    /// Run the full local pre-commit verification gate
    Gate {
        /// Run full whole-workspace checks (machete, deny, audit)
        #[arg(long)]
        full: bool,
        /// Do not abort on first step failure
        #[arg(long)]
        keep_going: bool,
    },

    /// Run all configured static quality checks in one go
    Check {
        /// Also run slow checks
        #[arg(long)]
        full: bool,
    },

    /// Manage Git hooks (pre-commit / pre-push) for cargo-warden
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum HookKindArg {
    PreCommit,
    PrePush,
}

impl From<HookKindArg> for hook::HookKind {
    fn from(val: HookKindArg) -> Self {
        match val {
            HookKindArg::PreCommit => hook::HookKind::PreCommit,
            HookKindArg::PrePush => hook::HookKind::PrePush,
        }
    }
}

#[derive(Subcommand, Debug)]
enum HookAction {
    /// Install or update a Git hook that runs cargo-warden
    Install {
        /// Git hook type to manage
        #[arg(long, value_enum, default_value_t = HookKindArg::PreCommit)]
        hook: HookKindArg,
        /// Run `cargo warden gate` instead of `cargo warden check`
        #[arg(long)]
        gate: bool,
        /// Run `cargo warden gate --full` (implies --gate)
        #[arg(long)]
        full: bool,
        /// Overwrite an existing unmanaged hook file
        #[arg(long)]
        force: bool,
        /// Append the cargo-warden block to an existing unmanaged hook file
        #[arg(long, conflicts_with = "force")]
        append: bool,
    },
    /// Remove the cargo-warden Git hook (or strip its managed block)
    Uninstall {
        /// Git hook type to remove
        #[arg(long, value_enum, default_value_t = HookKindArg::PreCommit)]
        hook: HookKindArg,
    },
    /// Show installation status of cargo-warden Git hooks
    Status,
}

#[derive(Subcommand, Debug)]
enum ThemeAction {
    /// Scan for unmapped hardcoded hex literals in .slint files
    Check {
        #[arg(long)]
        ui_dir: Option<PathBuf>,
    },
    /// Automatically rewrite hardcoded hex colors to Theme.c<hex>
    Apply {
        #[arg(long)]
        ui_dir: Option<PathBuf>,
    },
    /// Generate theme.slint from definition
    Generate {
        #[arg(long)]
        out_file: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum I18nAction {
    /// Verify that the generated Rust catalog is in sync with .po files
    Check {
        #[arg(long)]
        lang_dir: Option<PathBuf>,
        #[arg(long)]
        out_file: Option<PathBuf>,
    },
    /// Regenerate the Rust catalog lookup table from .po files
    Generate {
        #[arg(long)]
        lang_dir: Option<PathBuf>,
        #[arg(long)]
        out_file: Option<PathBuf>,
    },
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

    let config = load_config(cli.config.as_ref(), &base_dir)?;
    dispatch_command(cli.command, config, &base_dir)
}

fn dispatch_command(command: Commands, config: WardenConfig, base_dir: &Path) -> Result<()> {
    match command {
        Commands::Dag { root, forbid } => handle_dag(config, base_dir, root, forbid),
        Commands::CiParity { workflow, ignore } => {
            handle_ci_parity(config, base_dir, workflow, ignore)
        }
        Commands::Theme { action } => handle_theme(config, base_dir, action),
        Commands::I18n { action } => handle_i18n(config, base_dir, action),
        Commands::Complexity {
            max,
            paths,
            no_ignore_tests,
        } => handle_complexity(config, base_dir, max, paths, no_ignore_tests),
        Commands::FileLength {
            max_lines,
            ext,
            paths,
        } => handle_file_length(config, base_dir, max_lines, ext, paths),
        Commands::Gate { full, keep_going } => {
            let gate_cfg = config.gate.unwrap_or_default();
            gate::runner::run_gate(base_dir, &gate_cfg, full, keep_going)
        }
        Commands::Check { full: _ } => handle_check(&config, base_dir),
        Commands::Hook { action } => handle_hook(action, base_dir),
    }
}

fn handle_dag(
    config: WardenConfig,
    base_dir: &Path,
    root: Option<PathBuf>,
    forbid: Vec<String>,
) -> Result<()> {
    let dag_cfg = config.dag.unwrap_or_default();
    let roots = root.map_or(dag_cfg.roots, |r| vec![r]);
    let forbidden = if forbid.is_empty() {
        dag_cfg.forbid_imports
    } else {
        forbid
    };

    for r in roots {
        checks::dag::check_dag(&base_dir.join(r), &forbidden)?;
    }
    Ok(())
}

fn handle_ci_parity(
    config: WardenConfig,
    base_dir: &Path,
    workflow: Option<PathBuf>,
    ignore: Vec<String>,
) -> Result<()> {
    let ci_cfg = config.ci_parity.unwrap_or_default();
    let wf = workflow.unwrap_or(ci_cfg.workflow);
    let ignored = if ignore.is_empty() {
        ci_cfg.ignore_crates
    } else {
        ignore
    };

    checks::ci_parity::check_ci_parity(
        base_dir,
        &wf,
        &ci_cfg.test_patterns,
        &ignored,
        &ci_cfg.fanout_crates,
    )
}

fn handle_theme(config: WardenConfig, base_dir: &Path, action: ThemeAction) -> Result<()> {
    let theme_cfg = config.slint_theme.unwrap_or_default();
    match action {
        ThemeAction::Check { ui_dir } => {
            let dir = ui_dir.unwrap_or(theme_cfg.ui_dir);
            checks::slint_theme::check_theme(&base_dir.join(dir), &theme_cfg.unchanged_colors)
        }
        ThemeAction::Apply { ui_dir } => {
            let dir = ui_dir.unwrap_or(theme_cfg.ui_dir);
            let mapping = load_theme_token_mapping(base_dir, theme_cfg.theme_def.as_deref());
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

fn load_theme_token_mapping(
    base_dir: &Path,
    theme_def: Option<&Path>,
) -> std::collections::BTreeMap<String, String> {
    let mut mapping = std::collections::BTreeMap::new();
    let Some(def_path) = theme_def else {
        return mapping;
    };
    let Ok(content) = std::fs::read_to_string(base_dir.join(def_path)) else {
        return mapping;
    };
    let Ok(def) = toml::from_str::<checks::slint_theme::ThemeDefinition>(&content) else {
        return mapping;
    };

    for (k, _) in def.static_map {
        mapping.insert(k.to_ascii_lowercase(), format!("Theme.c{k}"));
    }
    for a in def.accent_map {
        mapping.insert(a.key.to_ascii_lowercase(), format!("Theme.c{}", a.key));
    }
    mapping
}

fn handle_i18n(config: WardenConfig, base_dir: &Path, action: I18nAction) -> Result<()> {
    let i18n_cfg = config.i18n.unwrap_or_default();
    match action {
        I18nAction::Check { lang_dir, out_file } => {
            let ldir = lang_dir.unwrap_or(i18n_cfg.lang_dir);
            let ofile = out_file.unwrap_or(i18n_cfg.out_file);
            checks::i18n_po::check_i18n(
                &base_dir.join(ldir),
                &i18n_cfg.domain,
                &i18n_cfg.locales,
                &base_dir.join(ofile),
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
) -> Result<()> {
    let comp_cfg = config.complexity.unwrap_or_default();
    let max_score = max.unwrap_or(comp_cfg.max);
    let raw_paths = if paths.is_empty() {
        &comp_cfg.paths
    } else {
        &paths
    };
    let search_paths = join_paths(base_dir, raw_paths);
    let ignore_tests = !no_ignore_tests && comp_cfg.ignore_tests;

    checks::complexity::check_complexity(&search_paths, max_score, ignore_tests, &comp_cfg.ignore)?;
    Ok(())
}

fn handle_file_length(
    config: WardenConfig,
    base_dir: &Path,
    max_lines: Option<usize>,
    ext: Vec<String>,
    paths: Vec<PathBuf>,
) -> Result<()> {
    let fl_cfg = config.file_length.unwrap_or_default();
    let limit = max_lines.unwrap_or(fl_cfg.max_lines);
    let extensions = if ext.is_empty() {
        fl_cfg.extensions
    } else {
        ext
    };
    let raw_paths = if paths.is_empty() {
        &fl_cfg.paths
    } else {
        &paths
    };
    let search_paths = join_paths(base_dir, raw_paths);

    checks::file_length::check_file_length(&search_paths, limit, &extensions, &fl_cfg.ignore)?;
    Ok(())
}

fn handle_check(config: &WardenConfig, base_dir: &Path) -> Result<()> {
    println!("🔍 [warden] running all configured static quality checks...");

    if let Some(fl_cfg) = &config.file_length {
        let search_paths = join_paths(base_dir, &fl_cfg.paths);
        checks::file_length::check_file_length(
            &search_paths,
            fl_cfg.max_lines,
            &fl_cfg.extensions,
            &fl_cfg.ignore,
        )?;
    }

    if let Some(comp_cfg) = &config.complexity {
        let search_paths = join_paths(base_dir, &comp_cfg.paths);
        checks::complexity::check_complexity(
            &search_paths,
            comp_cfg.max,
            comp_cfg.ignore_tests,
            &comp_cfg.ignore,
        )?;
    }

    if let Some(dag_cfg) = &config.dag {
        for r in &dag_cfg.roots {
            checks::dag::check_dag(&base_dir.join(r), &dag_cfg.forbid_imports)?;
        }
    }

    if let Some(ci_cfg) = &config.ci_parity {
        checks::ci_parity::check_ci_parity(
            base_dir,
            &ci_cfg.workflow,
            &ci_cfg.test_patterns,
            &ci_cfg.ignore_crates,
            &ci_cfg.fanout_crates,
        )?;
    }

    if let Some(theme_cfg) = &config.slint_theme
        && base_dir.join(&theme_cfg.ui_dir).exists()
    {
        checks::slint_theme::check_theme(
            &base_dir.join(&theme_cfg.ui_dir),
            &theme_cfg.unchanged_colors,
        )?;
    }

    if let Some(i18n_cfg) = &config.i18n
        && base_dir.join(&i18n_cfg.lang_dir).exists()
    {
        checks::i18n_po::check_i18n(
            &base_dir.join(&i18n_cfg.lang_dir),
            &i18n_cfg.domain,
            &i18n_cfg.locales,
            &base_dir.join(&i18n_cfg.out_file),
        )?;
    }

    println!("✨ [warden] all static quality checks passed successfully!");
    Ok(())
}

fn handle_hook(action: HookAction, base_dir: &Path) -> Result<()> {
    match action {
        HookAction::Install {
            hook: kind,
            gate,
            full,
            force,
            append,
        } => handle_hook_install(base_dir, kind.into(), gate, full, force, append),
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
    gate: bool,
    full: bool,
    force: bool,
    append: bool,
) -> Result<()> {
    let mode = if full {
        hook::HookMode::GateFull
    } else if gate {
        hook::HookMode::Gate
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

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use cargo_warden::config::WardenConfig;
use cargo_warden::{checks, gate};

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

    let base_dir = if let Some(cwd) = &cli.cwd {
        cwd.clone()
    } else {
        std::env::current_dir()?
    };
    tracing::debug!(base_dir = %base_dir.display(), "base directory resolved");

    let config = if let Some(cfg_path) = &cli.config {
        tracing::debug!(config_path = %cfg_path.display(), "loading explicit configuration file");
        let content = std::fs::read_to_string(cfg_path)?;
        toml::from_str(&content)?
    } else {
        tracing::debug!("auto-loading warden configuration");
        WardenConfig::load(&base_dir)?
    };

    match cli.command {
        Commands::Dag { root, forbid } => {
            let dag_cfg = config.dag.unwrap_or_default();
            let roots = if let Some(r) = root {
                vec![r]
            } else {
                dag_cfg.roots
            };
            let forbidden = if !forbid.is_empty() {
                forbid
            } else {
                dag_cfg.forbid_imports
            };

            for r in roots {
                let full_path = base_dir.join(r);
                checks::dag::check_dag(&full_path, &forbidden)?;
            }
        }

        Commands::CiParity { workflow, ignore } => {
            let ci_cfg = config.ci_parity.unwrap_or_default();
            let wf = workflow.unwrap_or(ci_cfg.workflow);
            let ignored = if !ignore.is_empty() {
                ignore
            } else {
                ci_cfg.ignore_crates
            };

            checks::ci_parity::check_ci_parity(
                &base_dir,
                &wf,
                &ci_cfg.test_patterns,
                &ignored,
                &ci_cfg.fanout_crates,
            )?;
        }

        Commands::Theme { action } => {
            let theme_cfg = config.slint_theme.unwrap_or_default();
            match action {
                ThemeAction::Check { ui_dir } => {
                    let dir = ui_dir.unwrap_or(theme_cfg.ui_dir);
                    checks::slint_theme::check_theme(
                        &base_dir.join(dir),
                        &theme_cfg.unchanged_colors,
                    )?;
                }
                ThemeAction::Apply { ui_dir } => {
                    let dir = ui_dir.unwrap_or(theme_cfg.ui_dir);
                    let mut mapping = std::collections::BTreeMap::new();
                    if let Some(def_path) = &theme_cfg.theme_def
                        && let Ok(content) = std::fs::read_to_string(base_dir.join(def_path))
                        && let Ok(def) =
                            toml::from_str::<checks::slint_theme::ThemeDefinition>(&content)
                    {
                        for (k, _) in def.static_map {
                            mapping.insert(k.to_ascii_lowercase(), format!("Theme.c{k}"));
                        }
                        for a in def.accent_map {
                            mapping.insert(a.key.to_ascii_lowercase(), format!("Theme.c{}", a.key));
                        }
                    }
                    checks::slint_theme::apply_tokens(
                        &base_dir.join(dir),
                        &mapping,
                        &theme_cfg.unchanged_colors,
                    )?;
                }
                ThemeAction::Generate { out_file } => {
                    let out = out_file.unwrap_or(theme_cfg.out_file);
                    let def = if let Some(def_path) = &theme_cfg.theme_def {
                        let content = std::fs::read_to_string(base_dir.join(def_path))?;
                        toml::from_str(&content)?
                    } else {
                        checks::slint_theme::ThemeDefinition::default()
                    };
                    checks::slint_theme::generate_theme(&def, &base_dir.join(out))?;
                }
            }
        }

        Commands::I18n { action } => {
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
                    )?;
                }
                I18nAction::Generate { lang_dir, out_file } => {
                    let ldir = lang_dir.unwrap_or(i18n_cfg.lang_dir);
                    let ofile = out_file.unwrap_or(i18n_cfg.out_file);
                    checks::i18n_po::sync_i18n(
                        &base_dir.join(ldir),
                        &i18n_cfg.domain,
                        &i18n_cfg.locales,
                        &base_dir.join(ofile),
                    )?;
                }
            }
        }

        Commands::Complexity {
            max,
            paths,
            no_ignore_tests,
        } => {
            let comp_cfg = config.complexity.unwrap_or_default();
            let max_score = max.unwrap_or(comp_cfg.max);
            let search_paths: Vec<PathBuf> = if !paths.is_empty() {
                paths.iter().map(|p| base_dir.join(p)).collect()
            } else {
                comp_cfg.paths.iter().map(|p| base_dir.join(p)).collect()
            };
            let ignore_tests = if no_ignore_tests {
                false
            } else {
                comp_cfg.ignore_tests
            };

            checks::complexity::check_complexity(
                &search_paths,
                max_score,
                ignore_tests,
                &comp_cfg.ignore,
            )?;
        }

        Commands::FileLength {
            max_lines,
            ext,
            paths,
        } => {
            let fl_cfg = config.file_length.unwrap_or_default();
            let limit = max_lines.unwrap_or(fl_cfg.max_lines);
            let extensions = if !ext.is_empty() {
                ext
            } else {
                fl_cfg.extensions
            };
            let search_paths: Vec<PathBuf> = if !paths.is_empty() {
                paths.iter().map(|p| base_dir.join(p)).collect()
            } else {
                fl_cfg.paths.iter().map(|p| base_dir.join(p)).collect()
            };

            checks::file_length::check_file_length(
                &search_paths,
                limit,
                &extensions,
                &fl_cfg.ignore,
            )?;
        }

        Commands::Gate { full, keep_going } => {
            let gate_cfg = config.gate.unwrap_or_default();
            gate::runner::run_gate(&base_dir, &gate_cfg, full, keep_going)?;
        }

        Commands::Check { full: _ } => {
            println!("🔍 [warden] running all configured static quality checks...");

            if let Some(fl_cfg) = &config.file_length {
                let search_paths: Vec<PathBuf> =
                    fl_cfg.paths.iter().map(|p| base_dir.join(p)).collect();
                checks::file_length::check_file_length(
                    &search_paths,
                    fl_cfg.max_lines,
                    &fl_cfg.extensions,
                    &fl_cfg.ignore,
                )?;
            }

            if let Some(comp_cfg) = &config.complexity {
                let search_paths: Vec<PathBuf> =
                    comp_cfg.paths.iter().map(|p| base_dir.join(p)).collect();
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
                    &base_dir,
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
        }
    }

    Ok(())
}

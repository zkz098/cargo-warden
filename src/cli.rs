use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use cargo_warden::hook;
use cargo_warden::report::OutputFormat;

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum OutputFormatArg {
    Text,
    Json,
    Sarif,
    GithubActions,
}

impl From<OutputFormatArg> for OutputFormat {
    fn from(val: OutputFormatArg) -> Self {
        match val {
            OutputFormatArg::Text => OutputFormat::Text,
            OutputFormatArg::Json => OutputFormat::Json,
            OutputFormatArg::Sarif => OutputFormat::Sarif,
            OutputFormatArg::GithubActions => OutputFormat::GithubActions,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "cargo-warden",
    bin_name = "cargo warden",
    version,
    about = "Architecture boundaries, quality gates, cognitive complexity and drift prevention",
    long_about = "A unified Cargo plugin and code quality toolkit for Rust and Slint projects."
)]
pub struct Cli {
    /// Custom path to warden.toml configuration file
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Working directory (defaults to current directory)
    #[arg(short = 'C', long, global = true)]
    pub cwd: Option<PathBuf>,

    /// Output format for diagnostics
    #[arg(long, value_enum, global = true, default_value_t = OutputFormatArg::Text)]
    pub format: OutputFormatArg,

    /// Check only staged files (`git diff --cached`)
    #[arg(long, global = true, conflicts_with = "diff")]
    pub staged: bool,

    /// Check only files changed relative to a Git ref (e.g. `origin/main`)
    #[arg(long, value_name = "BASE", global = true, conflicts_with = "staged")]
    pub diff: Option<String>,

    /// Override baseline file path
    #[arg(
        long,
        value_name = "PATH",
        global = true,
        conflicts_with = "no_baseline"
    )]
    pub baseline: Option<PathBuf>,

    /// Ignore any configured baseline file
    #[arg(long, global = true)]
    pub no_baseline: bool,

    /// Increase logging verbosity (-v for info, -vv for debug, -vvv for trace)
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Suppress non-essential logging output
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize a warden.toml configuration by inspecting the repository
    Init {
        /// Overwrite an existing configuration file
        #[arg(long)]
        force: bool,
        /// Write `.warden.toml` instead of `warden.toml`
        #[arg(long)]
        dotfile: bool,
        /// Prompt interactively for key thresholds and checks
        #[arg(short = 'i', long)]
        interactive: bool,
    },

    /// Manage baseline snapshots (`warden-baseline.json`)
    Baseline {
        #[command(subcommand)]
        action: BaselineAction,
    },

    /// Verify module dependency boundaries, layered rules, and detect dependency cycles
    Dag {
        /// Source directory to inspect (e.g. `src` or `crates/core/src`)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Top-level modules that must not be imported
        #[arg(long)]
        forbid: Vec<String>,
        /// Run workspace crate-level dependency boundary checks (`[dag.workspace]`)
        #[arg(long)]
        workspace: bool,
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

    /// Scan Rust functions for cognitive complexity, length, and nesting violations
    Complexity {
        /// Maximum allowed cognitive complexity score per function
        #[arg(long)]
        max: Option<usize>,
        /// Maximum allowed lines per function/method
        #[arg(long)]
        max_lines: Option<usize>,
        /// Maximum allowed control-flow nesting depth per function/method
        #[arg(long)]
        max_nesting: Option<usize>,
        /// Paths or files to inspect
        #[arg(long)]
        paths: Vec<PathBuf>,
        /// Do not ignore test files
        #[arg(long)]
        no_ignore_tests: bool,
        /// Do not respect `.gitignore` rules
        #[arg(long)]
        no_gitignore: bool,
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
        /// Count effective code lines (SLOC), excluding blank lines and comments
        #[arg(long, alias = "sloc")]
        effective: bool,
        /// Do not respect `.gitignore` rules
        #[arg(long)]
        no_gitignore: bool,
    },

    /// Run the full local verification gate (static checks + fmt + clippy + test + doc)
    Gate {
        /// Run full whole-workspace checks (machete, deny, audit)
        #[arg(long)]
        full: bool,
        /// Do not abort on first step failure
        #[arg(long)]
        keep_going: bool,
        /// Skip static quality checks (`warden check`) before Cargo steps
        #[arg(long)]
        skip_checks: bool,
        /// Automatically apply safe fixes (slint theme, i18n sync, clippy --fix, cargo fmt, custom fix steps)
        #[arg(long)]
        fix: bool,
        /// Run independent read-only steps in parallel
        #[arg(long, conflicts_with = "no_parallel")]
        parallel: bool,
        /// Force sequential execution of all checks and gate steps
        #[arg(long, conflicts_with = "parallel")]
        no_parallel: bool,
        /// Run only the specified step(s) (comma-separated: checks,fmt,clippy,test,doc,machete,deny,audit, or custom step names)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Skip the specified step(s) (comma-separated)
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
    },

    /// Run all configured static quality checks (and optional toolchain checks)
    Check {
        /// Also run fmt, clippy, and doc checks (skips test suite)
        #[arg(long)]
        full: bool,
        /// Automatically apply safe fixes (slint theme, i18n sync, clippy --fix, cargo fmt)
        #[arg(long)]
        fix: bool,
        /// Run independent read-only static checks in parallel
        #[arg(long, conflicts_with = "no_parallel")]
        parallel: bool,
        /// Force sequential execution of static checks
        #[arg(long, conflicts_with = "parallel")]
        no_parallel: bool,
    },

    /// Manage Git hooks (pre-commit / pre-push) for cargo-warden
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },

    /// Diagnose cargo-warden configuration, toolchain, gate tools, and Git hooks
    Doctor {
        /// Treat warnings (such as missing config or unpinned toolchains) as failures
        #[arg(long)]
        strict: bool,
    },

    /// Generate shell completion scripts for bash, zsh, fish, powershell, or elvish
    Completions {
        /// Target shell
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand, Debug)]
pub enum BaselineAction {
    /// Capture current violations into a baseline JSON file
    Generate {
        /// Target baseline output path (defaults to config `baseline` or `warden-baseline.json`)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Verify the project against the configured or given baseline file
    Check {
        /// Baseline file path to check against
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Remove stale baseline entries that no longer match any active violation
    Prune {
        /// Baseline file path to prune (defaults to config `baseline` or `warden-baseline.json`)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Report stale entries without modifying the baseline file
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum HookKindArg {
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
pub enum HookAction {
    /// Install or update a Git hook that runs cargo-warden
    Install {
        /// Git hook type to manage
        #[arg(long, value_enum, default_value_t = HookKindArg::PreCommit)]
        hook: HookKindArg,
        /// Run `cargo warden check --staged` (fast incremental pre-commit check)
        #[arg(long, conflicts_with_all = ["gate", "full"])]
        staged: bool,
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
pub enum ThemeAction {
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
pub enum I18nAction {
    /// Verify that the generated Rust catalog, .po quality, and source coverage are valid
    Check {
        #[arg(long)]
        lang_dir: Option<PathBuf>,
        #[arg(long)]
        out_file: Option<PathBuf>,
        /// Source directories to scan for `@tr(...)` and `tr(...)` / `tr!(...)` keys
        #[arg(long)]
        source_dirs: Vec<PathBuf>,
        /// Fail on empty `msgstr`, `#, fuzzy` entries, or missing cross-locale keys
        #[arg(long)]
        strict: bool,
        /// Flag `.po` entries not referenced in `source_dirs`
        #[arg(long)]
        check_unused: bool,
        /// Disable placeholder consistency validation (`{}`, `{0}`, `%s`, etc.)
        #[arg(long)]
        no_placeholders: bool,
    },
    /// Regenerate the Rust catalog lookup table from .po files
    Generate {
        #[arg(long)]
        lang_dir: Option<PathBuf>,
        #[arg(long)]
        out_file: Option<PathBuf>,
    },
}

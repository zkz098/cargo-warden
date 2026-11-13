use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WardenConfig {
    /// Optional path to baseline JSON file (e.g. "warden-baseline.json")
    pub baseline: Option<PathBuf>,
    pub dag: Option<DagConfig>,
    pub ci_parity: Option<CiParityConfig>,
    pub slint_theme: Option<SlintThemeConfig>,
    pub i18n: Option<I18nConfig>,
    pub complexity: Option<ComplexityConfig>,
    pub file_length: Option<FileLengthConfig>,
    pub check: Option<CheckConfig>,
    pub gate: Option<GateConfig>,
}

/// Optional Cargo toolchain checks integrated into `cargo warden check`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckConfig {
    /// Run `cargo fmt --all -- --check` during `cargo warden check`
    pub fmt: bool,
    /// Run `cargo clippy` during `cargo warden check`
    pub clippy: bool,
    /// Run `cargo doc` during `cargo warden check`
    pub doc: bool,
    /// Run independent read-only static checks in parallel (default: true)
    #[serde(default = "default_true")]
    pub parallel: bool,
    /// Emit warnings for unused inline `// warden:allow(...)` directives (default: true)
    #[serde(default = "default_true")]
    pub warn_unused_suppressions: bool,
}

impl Default for CheckConfig {
    fn default() -> Self {
        Self {
            fmt: false,
            clippy: false,
            doc: false,
            parallel: true,
            warn_unused_suppressions: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DagConfig {
    /// Source directories to scan for modules (e.g. ["crates/my-core/src"])
    pub roots: Vec<PathBuf>,
    /// Top-level modules that must never be imported by other sibling modules
    pub forbid_imports: Vec<String>,
    /// Named architectural layers with allowed/denied dependency targets
    pub layers: Vec<LayerConfig>,
    /// Explicit module/layer dependency rules
    pub rules: Vec<DagRuleConfig>,
    /// Workspace crate-level dependency boundary configuration
    pub workspace: Option<WorkspaceDagConfig>,
}

impl Default for DagConfig {
    fn default() -> Self {
        Self {
            roots: vec![PathBuf::from("src")],
            forbid_imports: Vec::new(),
            layers: Vec::new(),
            rules: Vec::new(),
            workspace: None,
        }
    }
}

/// Workspace-level crate dependency boundary configuration (`[dag.workspace]`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceDagConfig {
    /// Enable workspace crate dependency checks even when only checking cycles
    pub enabled: bool,
    /// Include `[dev-dependencies]` edges when checking workspace crate boundaries
    pub include_dev_dependencies: bool,
    /// Include `[build-dependencies]` edges when checking workspace crate boundaries
    pub include_build_dependencies: bool,
    /// Crates (workspace or external) that no other workspace member may depend on
    pub forbid_deps: Vec<String>,
    /// Warn when multiple distinct versions of the same external crate exist in resolved metadata
    pub check_version_conflicts: bool,
    /// Named architectural layers grouping workspace crates
    pub layers: Vec<WorkspaceLayerConfig>,
    /// Explicit crate or layer dependency rules
    pub rules: Vec<WorkspaceDagRuleConfig>,
}

impl WorkspaceDagConfig {
    /// Returns `true` if any workspace-level crate DAG check is configured or explicitly enabled.
    pub fn is_active(&self) -> bool {
        self.enabled
            || !self.forbid_deps.is_empty()
            || !self.layers.is_empty()
            || !self.rules.is_empty()
            || self.check_version_conflicts
    }
}

/// Architectural layer definition for workspace crate dependencies.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceLayerConfig {
    pub name: String,
    /// Crate names or glob patterns belonging to this layer (defaults to `[name]`)
    pub crates: Vec<String>,
    /// Allowed target layers or workspace crates (`None` = unrestricted unless denied)
    pub allow: Option<Vec<String>>,
    /// Explicitly forbidden target layers, workspace crates, or external crates
    pub deny: Vec<String>,
}

/// Direct dependency boundary rule between workspace crates or layers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceDagRuleConfig {
    pub from: String,
    /// Allowed target workspace crates or layers (`None` = unrestricted unless denied)
    pub allow_only: Option<Vec<String>>,
    /// Forbidden target layers, workspace crates, or external crates
    pub deny: Vec<String>,
}

/// Architectural layer definition for `dag` boundary enforcement.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LayerConfig {
    pub name: String,
    /// Module names or `parent::child` paths belonging to this layer (defaults to `[name]`)
    pub modules: Vec<String>,
    /// Allowed target layers or modules (if `Some`, any other cross-layer dependency is denied)
    pub allow: Option<Vec<String>>,
    /// Explicitly forbidden target layers or modules
    pub deny: Vec<String>,
}

/// Direct dependency boundary rule between modules or layers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DagRuleConfig {
    pub from: String,
    /// Allowed target modules or layers (if `Some`, all other targets are denied)
    pub allow_only: Option<Vec<String>>,
    /// Forbidden target modules or layers
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CiParityConfig {
    /// Relative path to CI workflow file (default: ".github/workflows/ci.yml")
    pub workflow: PathBuf,
    /// Patterns to match test command lines in CI (default: ["nextest", "llvm-cov"])
    pub test_patterns: Vec<String>,
    /// Crates to ignore during parity check
    pub ignore_crates: Vec<String>,
    /// Suffix list of crates expected in the coverage fanout loop
    pub fanout_crates: Vec<String>,
}

impl Default for CiParityConfig {
    fn default() -> Self {
        Self {
            workflow: PathBuf::from(".github/workflows/ci.yml"),
            test_patterns: vec!["nextest".to_string(), "llvm-cov".to_string()],
            ignore_crates: Vec::new(),
            fanout_crates: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlintThemeConfig {
    /// Directory containing .slint files (default: "ui")
    pub ui_dir: PathBuf,
    /// Target generated theme slint file
    pub out_file: PathBuf,
    /// Hex colors that are allowed to remain literal (whitelist)
    pub unchanged_colors: Vec<String>,
    /// Theme color mapping source file (optional TOML with dark->light and accents)
    pub theme_def: Option<PathBuf>,
    /// Respect `.gitignore` rules while scanning UI directory
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
}

impl Default for SlintThemeConfig {
    fn default() -> Self {
        Self {
            ui_dir: PathBuf::from("ui"),
            out_file: PathBuf::from("ui/theme.slint"),
            unchanged_colors: vec![
                "ffffff".to_string(),
                "000000".to_string(),
                "6b7280".to_string(),
            ],
            theme_def: None,
            respect_gitignore: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct I18nConfig {
    /// Directory holding `<locale>/LC_MESSAGES/*.po` files
    pub lang_dir: PathBuf,
    /// Output generated Rust catalog path
    pub out_file: PathBuf,
    /// PO file name without extension (default: "messages")
    pub domain: String,
    /// Locales order for catalog array
    pub locales: Vec<String>,
    /// Source directories to scan for `@tr(...)` (Slint) and `tr(...)` / `tr!(...)` (Rust) keys
    pub source_dirs: Vec<PathBuf>,
    /// Rust function/macro names treated as translation calls (default: `["tr", "t", "gettext"]`)
    #[serde(default = "default_tr_macros")]
    pub tr_macros: Vec<String>,
    /// Fail when any `.po` entry is empty (`msgstr ""`) or marked `#, fuzzy`
    pub strict: bool,
    /// Verify placeholder consistency (`{}`, `{0}`, `{name}`, `%s`, etc.) between `msgid` and `msgstr`
    #[serde(default = "default_true")]
    pub check_placeholders: bool,
    /// Flag `.po` entries that are no longer referenced by any file in `source_dirs`
    pub check_unused: bool,
    /// Respect `.gitignore` rules when scanning `source_dirs`
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
}

fn default_tr_macros() -> Vec<String> {
    vec!["tr".to_string(), "t".to_string(), "gettext".to_string()]
}

impl Default for I18nConfig {
    fn default() -> Self {
        Self {
            lang_dir: PathBuf::from("lang"),
            out_file: PathBuf::from("src/i18n/catalog.rs"),
            domain: "messages".to_string(),
            locales: vec!["en".to_string()],
            source_dirs: Vec::new(),
            tr_macros: default_tr_macros(),
            strict: false,
            check_placeholders: true,
            check_unused: false,
            respect_gitignore: true,
        }
    }
}

/// Path-scoped threshold override for `complexity`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ComplexityOverride {
    /// Relative path prefixes or glob patterns (e.g. `crates/parser/**`, `src/legacy.rs`)
    pub paths: Vec<String>,
    /// Override maximum allowed cognitive complexity score
    pub max: Option<usize>,
    /// Override maximum allowed function line count
    pub max_function_lines: Option<usize>,
    /// Override maximum allowed control-flow nesting depth
    pub max_nesting: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ComplexityConfig {
    /// Maximum allowed cognitive complexity per function/method
    pub max: usize,
    /// Optional maximum allowed lines per function/method
    pub max_function_lines: Option<usize>,
    /// Optional maximum allowed nesting depth per function/method
    pub max_nesting: Option<usize>,
    /// Roots/paths to scan (default: ["."])
    pub paths: Vec<PathBuf>,
    /// Exclude test files (*_test.rs, tests.rs, tests/ directory)
    pub ignore_tests: bool,
    /// File patterns or paths to ignore (supports glob patterns)
    pub ignore: Vec<String>,
    /// Respect `.gitignore` rules while scanning
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
    /// Path-scoped threshold overrides (evaluated in order; last match wins)
    pub overrides: Vec<ComplexityOverride>,
}

impl Default for ComplexityConfig {
    fn default() -> Self {
        Self {
            max: 20,
            max_function_lines: None,
            max_nesting: None,
            paths: vec![PathBuf::from(".")],
            ignore_tests: true,
            ignore: vec!["target".to_string()],
            respect_gitignore: true,
            overrides: Vec::new(),
        }
    }
}

/// Line counting mode for `file_length`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LineCountMode {
    /// Count all physical lines (`content.lines().count()`).
    #[default]
    Physical,
    /// Count only effective code lines (SLOC), excluding blank lines and comments.
    Effective,
}

/// Path-scoped threshold override for `file_length`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FileLengthOverride {
    /// Relative path prefixes or glob patterns (e.g. `crates/ui/**`, `*.slint`)
    pub paths: Vec<String>,
    /// Override maximum allowed line count
    pub max_lines: Option<usize>,
    /// Override line counting mode (`physical` or `effective`)
    pub mode: Option<LineCountMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileLengthConfig {
    /// Maximum line count for a file
    pub max_lines: usize,
    /// File extensions to check (e.g. ["rs", "slint", "toml"])
    pub extensions: Vec<String>,
    /// Roots/paths to scan
    pub paths: Vec<PathBuf>,
    /// Ignore patterns (supports glob patterns)
    pub ignore: Vec<String>,
    /// Line counting mode (`physical` or `effective`)
    pub mode: LineCountMode,
    /// Respect `.gitignore` rules while scanning
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
    /// Path-scoped threshold overrides (evaluated in order; last match wins)
    pub overrides: Vec<FileLengthOverride>,
}

impl Default for FileLengthConfig {
    fn default() -> Self {
        Self {
            max_lines: 800,
            extensions: vec!["rs".to_string(), "slint".to_string()],
            paths: vec![PathBuf::from(".")],
            ignore: vec!["target".to_string(), "tests".to_string()],
            mode: LineCountMode::Physical,
            respect_gitignore: true,
            overrides: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GateConfig {
    /// Run configured static quality checks (`warden check`) before Cargo steps (default: true)
    #[serde(default = "default_true")]
    pub include_checks: bool,
    /// Run independent read-only steps in parallel (default: true)
    #[serde(default = "default_true")]
    pub parallel: bool,
    /// Groups of packages to test
    pub test_groups: Vec<TestGroupConfig>,
    /// Pinned rustfmt version (optional)
    pub rustfmt_toolchain: Option<String>,
    /// Extra/override arguments passed to `cargo clippy` (defaults to `--workspace --all-targets -- -D warnings`)
    pub clippy_args: Vec<String>,
    /// Extra/override arguments passed to `cargo doc` (defaults to `--workspace --no-deps`)
    pub doc_args: Vec<String>,
    /// Custom `RUSTDOCFLAGS` for the `doc` step (defaults to `-D warnings`)
    pub rustdocflags: Option<String>,
    /// Step names to skip by default (e.g. `["audit"]`)
    pub skip_steps: Vec<String>,
    /// User-defined custom steps executed during `cargo warden gate`
    pub custom_steps: Vec<CustomStepConfig>,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            include_checks: true,
            parallel: true,
            test_groups: Vec::new(),
            rustfmt_toolchain: None,
            clippy_args: Vec::new(),
            doc_args: Vec::new(),
            rustdocflags: None,
            skip_steps: Vec::new(),
            custom_steps: Vec::new(),
        }
    }
}

/// User-defined custom command step executed by `cargo warden gate`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CustomStepConfig {
    /// Step name (usable in `--only`, `--skip`, and `skip_steps`)
    pub name: String,
    /// Executable program name or path (optional if `command` is provided)
    pub program: String,
    /// Arguments passed to `program`
    pub args: Vec<String>,
    /// Shell/command string shorthand (used when `program` is empty)
    pub command: Option<String>,
    /// Optional cargo subcommand tool name to check via `cargo <tool> --version` before running
    pub cargo_tool: Option<String>,
    /// Environment variables set for this step
    pub env: BTreeMap<String, String>,
    /// Working directory relative to repository root
    pub cwd: Option<PathBuf>,
    /// Run only when `--full` is passed to `cargo warden gate`
    pub full_only: bool,
    /// Mark step as read-only so consecutive read-only steps can run in parallel
    pub read_only: bool,
    /// Replacement arguments passed to `program` when `--fix` is active
    pub fix_args: Option<Vec<String>>,
    /// Replacement command string used when `--fix` is active
    pub fix_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGroupConfig {
    pub name: String,
    pub packages: Vec<String>,
    pub features: Vec<String>,
}

/// Origin from which a `WardenConfig` was loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Loaded from `warden.toml` or `.warden.toml`.
    File(PathBuf),
    /// Loaded from `[workspace.metadata.warden]` in `Cargo.toml`.
    CargoMetadata(PathBuf),
}

impl WardenConfig {
    /// Try to locate and load `warden.toml`, `.warden.toml`, or `[workspace.metadata.warden]`.
    pub fn load(root: &Path) -> Result<Self> {
        Ok(Self::load_with_source(root)?.0)
    }

    /// Locate and load configuration along with its discovered source, if any.
    pub fn load_with_source(root: &Path) -> Result<(Self, Option<ConfigSource>)> {
        let candidates = [root.join("warden.toml"), root.join(".warden.toml")];

        for path in &candidates {
            if path.is_file() {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("Failed to read config file {}", path.display()))?;
                let config: WardenConfig = toml::from_str(&content)
                    .with_context(|| format!("Failed to parse TOML in {}", path.display()))?;
                return Ok((config, Some(ConfigSource::File(path.clone()))));
            }
        }

        // Try Cargo.toml metadata [workspace.metadata.warden]
        let cargo_toml = root.join("Cargo.toml");
        if cargo_toml.is_file()
            && let Ok(content) = std::fs::read_to_string(&cargo_toml)
            && let Ok(value) = toml::from_str::<toml::Value>(&content)
            && let Some(metadata) = value
                .get("workspace")
                .and_then(|w| w.get("metadata"))
                .and_then(|m| m.get("warden"))
            && let Ok(config) = metadata.clone().try_into()
        {
            return Ok((config, Some(ConfigSource::CargoMetadata(cargo_toml))));
        }

        Ok((WardenConfig::default(), None))
    }
}

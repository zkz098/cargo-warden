use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CheckConfig {
    /// Run `cargo fmt --all -- --check` during `cargo warden check`
    pub fmt: bool,
    /// Run `cargo clippy` during `cargo warden check`
    pub clippy: bool,
    /// Run `cargo doc` during `cargo warden check`
    pub doc: bool,
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
}

impl Default for DagConfig {
    fn default() -> Self {
        Self {
            roots: vec![PathBuf::from("src")],
            forbid_imports: Vec::new(),
            layers: Vec::new(),
            rules: Vec::new(),
        }
    }
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
}

impl Default for I18nConfig {
    fn default() -> Self {
        Self {
            lang_dir: PathBuf::from("lang"),
            out_file: PathBuf::from("src/i18n/catalog.rs"),
            domain: "messages".to_string(),
            locales: vec!["en".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ComplexityConfig {
    /// Maximum allowed cognitive complexity per function/method
    pub max: usize,
    /// Roots/paths to scan (default: ["."])
    pub paths: Vec<PathBuf>,
    /// Exclude test files (*_test.rs, tests.rs, tests/ directory)
    pub ignore_tests: bool,
    /// File patterns or paths to ignore (supports glob patterns)
    pub ignore: Vec<String>,
    /// Respect `.gitignore` rules while scanning
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
}

impl Default for ComplexityConfig {
    fn default() -> Self {
        Self {
            max: 20,
            paths: vec![PathBuf::from(".")],
            ignore_tests: true,
            ignore: vec!["target".to_string()],
            respect_gitignore: true,
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
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            include_checks: true,
            test_groups: Vec::new(),
            rustfmt_toolchain: None,
            clippy_args: Vec::new(),
            doc_args: Vec::new(),
            rustdocflags: None,
            skip_steps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGroupConfig {
    pub name: String,
    pub packages: Vec<String>,
    pub features: Vec<String>,
}

impl WardenConfig {
    /// Try to locate and load `warden.toml` or `.warden.toml` in `root` or parent dirs.
    pub fn load(root: &Path) -> Result<Self> {
        let candidates = [root.join("warden.toml"), root.join(".warden.toml")];

        for path in &candidates {
            if path.is_file() {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("Failed to read config file {}", path.display()))?;
                let config: WardenConfig = toml::from_str(&content)
                    .with_context(|| format!("Failed to parse TOML in {}", path.display()))?;
                return Ok(config);
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
            return Ok(config);
        }

        Ok(WardenConfig::default())
    }
}

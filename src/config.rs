use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WardenConfig {
    pub dag: Option<DagConfig>,
    pub ci_parity: Option<CiParityConfig>,
    pub slint_theme: Option<SlintThemeConfig>,
    pub i18n: Option<I18nConfig>,
    pub complexity: Option<ComplexityConfig>,
    pub file_length: Option<FileLengthConfig>,
    pub gate: Option<GateConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DagConfig {
    /// Source directories to scan for modules (e.g. ["crates/my-core/src"])
    pub roots: Vec<PathBuf>,
    /// Top-level modules that must never be imported by other sibling modules
    pub forbid_imports: Vec<String>,
}

impl Default for DagConfig {
    fn default() -> Self {
        Self {
            roots: vec![PathBuf::from("src")],
            forbid_imports: Vec::new(),
        }
    }
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
    /// File patterns or paths to ignore
    pub ignore: Vec<String>,
}

impl Default for ComplexityConfig {
    fn default() -> Self {
        Self {
            max: 20,
            paths: vec![PathBuf::from(".")],
            ignore_tests: true,
            ignore: vec!["target".to_string()],
        }
    }
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
    /// Ignore patterns
    pub ignore: Vec<String>,
}

impl Default for FileLengthConfig {
    fn default() -> Self {
        Self {
            max_lines: 800,
            extensions: vec!["rs".to_string(), "slint".to_string()],
            paths: vec![PathBuf::from(".")],
            ignore: vec!["target".to_string(), "tests".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GateConfig {
    /// Groups of packages to test
    pub test_groups: Vec<TestGroupConfig>,
    /// Pinned rustfmt version (optional)
    pub rustfmt_toolchain: Option<String>,
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

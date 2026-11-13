//! Environment and configuration diagnostic command (`cargo warden doctor`).

use crate::config::{ConfigSource, WardenConfig};
use crate::gate::steps::{tool_is_installed, toolchain_is_installed};
use crate::hook::{self, HookState};
use crate::report::OutputFormat;
use anyhow::{Result, bail};
use serde::Serialize;
use std::path::Path;
use std::process::Command;

/// Health status of a single `doctor` check item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DoctorStatus {
    Ok,
    Info,
    Warn,
    Error,
}

/// One diagnostic entry emitted by `cargo warden doctor`.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorItem {
    pub category: String,
    pub name: String,
    pub status: DoctorStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Summary counts for a `doctor` run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DoctorSummary {
    pub ok: usize,
    pub info: usize,
    pub warnings: usize,
    pub errors: usize,
}

/// Full report returned by `cargo warden doctor`.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub healthy: bool,
    pub summary: DoctorSummary,
    pub checks: Vec<DoctorItem>,
}

impl DoctorReport {
    pub fn from_items(checks: Vec<DoctorItem>, strict: bool) -> Self {
        let mut summary = DoctorSummary::default();
        for item in &checks {
            match item.status {
                DoctorStatus::Ok => summary.ok += 1,
                DoctorStatus::Info => summary.info += 1,
                DoctorStatus::Warn => summary.warnings += 1,
                DoctorStatus::Error => summary.errors += 1,
            }
        }
        let healthy = summary.errors == 0 && (!strict || summary.warnings == 0);
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            healthy,
            summary,
            checks,
        }
    }
}

/// Collect environment, configuration, toolchain, and Git hook diagnostics.
pub fn collect_doctor_report(
    base_dir: &Path,
    explicit_config: Option<&Path>,
    strict: bool,
) -> DoctorReport {
    let mut items = Vec::new();
    let config = check_configuration(base_dir, explicit_config, &mut items);
    check_core_toolchain(config.as_ref(), &mut items);
    check_optional_gate_tools(config.as_ref(), &mut items);
    if let Some(cfg) = &config {
        check_configured_paths(base_dir, cfg, &mut items);
    }
    check_git_hooks(base_dir, &mut items);
    DoctorReport::from_items(items, strict)
}

/// Run `cargo warden doctor` and emit formatted output.
pub fn run_doctor(
    base_dir: &Path,
    explicit_config: Option<&Path>,
    strict: bool,
    format: OutputFormat,
) -> Result<()> {
    let report = collect_doctor_report(base_dir, explicit_config, strict);
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        _ => print_text_report(&report),
    }

    if !report.healthy {
        bail!(
            "Doctor found {} error(s) and {} warning(s)",
            report.summary.errors,
            report.summary.warnings
        );
    }
    Ok(())
}

fn print_text_report(report: &DoctorReport) {
    println!(
        "🩺 [doctor] cargo-warden v{} environment check",
        report.version
    );
    for item in &report.checks {
        let icon = match item.status {
            DoctorStatus::Ok => "✅",
            DoctorStatus::Info => "ℹ️ ",
            DoctorStatus::Warn => "⚠️ ",
            DoctorStatus::Error => "❌",
        };
        println!(
            "  {icon} [{}] {}: {}",
            item.category, item.name, item.detail
        );
        if let Some(hint) = &item.hint {
            println!("       hint: {hint}");
        }
    }
    println!(
        "Summary: {} ok, {} info, {} warning(s), {} error(s)",
        report.summary.ok, report.summary.info, report.summary.warnings, report.summary.errors
    );
}

fn check_configuration(
    base_dir: &Path,
    explicit_config: Option<&Path>,
    items: &mut Vec<DoctorItem>,
) -> Option<WardenConfig> {
    if let Some(cfg_path) = explicit_config {
        match std::fs::read_to_string(cfg_path)
            .map_err(anyhow::Error::from)
            .and_then(|s| toml::from_str::<WardenConfig>(&s).map_err(anyhow::Error::from))
        {
            Ok(cfg) => {
                items.push(DoctorItem {
                    category: "config".to_string(),
                    name: "configuration".to_string(),
                    status: DoctorStatus::Ok,
                    detail: format!("loaded {}", cfg_path.display()),
                    hint: None,
                });
                return Some(cfg);
            }
            Err(err) => {
                items.push(DoctorItem {
                    category: "config".to_string(),
                    name: "configuration".to_string(),
                    status: DoctorStatus::Error,
                    detail: format!("failed to load {}: {err}", cfg_path.display()),
                    hint: Some("fix syntax/schema errors in the config file".to_string()),
                });
                return None;
            }
        }
    }

    match WardenConfig::load_with_source(base_dir) {
        Ok((cfg, Some(ConfigSource::File(path)))) => {
            items.push(DoctorItem {
                category: "config".to_string(),
                name: "configuration".to_string(),
                status: DoctorStatus::Ok,
                detail: format!("loaded {}", path.display()),
                hint: None,
            });
            Some(cfg)
        }
        Ok((cfg, Some(ConfigSource::CargoMetadata(path)))) => {
            items.push(DoctorItem {
                category: "config".to_string(),
                name: "configuration".to_string(),
                status: DoctorStatus::Ok,
                detail: format!("loaded [workspace.metadata.warden] from {}", path.display()),
                hint: None,
            });
            Some(cfg)
        }
        Ok((_, None)) => {
            items.push(DoctorItem {
                category: "config".to_string(),
                name: "configuration".to_string(),
                status: DoctorStatus::Warn,
                detail: "no warden.toml, .warden.toml, or [workspace.metadata.warden] found"
                    .to_string(),
                hint: Some("run `cargo warden init` to generate a configuration".to_string()),
            });
            None
        }
        Err(err) => {
            items.push(DoctorItem {
                category: "config".to_string(),
                name: "configuration".to_string(),
                status: DoctorStatus::Error,
                detail: format!("{err}"),
                hint: Some("fix syntax errors in your configuration file".to_string()),
            });
            None
        }
    }
}

fn check_core_toolchain(config: Option<&WardenConfig>, items: &mut Vec<DoctorItem>) {
    check_program_version(
        "toolchain",
        "cargo",
        &["--version"],
        DoctorStatus::Error,
        None,
        items,
    );
    check_program_version(
        "toolchain",
        "rustc",
        &["--version"],
        DoctorStatus::Error,
        None,
        items,
    );
    check_program_version(
        "toolchain",
        "rustfmt",
        &["fmt", "--version"],
        DoctorStatus::Warn,
        Some("install with `rustup component add rustfmt`"),
        items,
    );
    check_program_version(
        "toolchain",
        "clippy",
        &["clippy", "--version"],
        DoctorStatus::Warn,
        Some("install with `rustup component add clippy`"),
        items,
    );
    check_program_version(
        "toolchain",
        "git",
        &["--version"],
        DoctorStatus::Warn,
        Some("install Git to use `--staged`, `--diff`, and `hook` commands"),
        items,
    );

    if let Some(tc) = config
        .and_then(|c| c.gate.as_ref())
        .and_then(|g| g.rustfmt_toolchain.as_deref())
    {
        if toolchain_is_installed(tc) {
            items.push(DoctorItem {
                category: "toolchain".to_string(),
                name: format!("rustfmt ({tc})"),
                status: DoctorStatus::Ok,
                detail: format!("toolchain `{tc}` is installed"),
                hint: None,
            });
        } else {
            items.push(DoctorItem {
                category: "toolchain".to_string(),
                name: format!("rustfmt ({tc})"),
                status: DoctorStatus::Warn,
                detail: format!("pinned toolchain `{tc}` is not installed"),
                hint: Some(format!(
                    "install with `rustup toolchain install {tc} --component rustfmt`"
                )),
            });
        }
    }
}

fn check_program_version(
    category: &str,
    name: &str,
    args: &[&str],
    missing_status: DoctorStatus,
    hint: Option<&str>,
    items: &mut Vec<DoctorItem>,
) {
    let program = if name == "rustfmt" || name == "clippy" {
        "cargo"
    } else {
        name
    };
    match Command::new(program).args(args).output() {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
            items.push(DoctorItem {
                category: category.to_string(),
                name: name.to_string(),
                status: DoctorStatus::Ok,
                detail: if ver.is_empty() {
                    "available".to_string()
                } else {
                    ver
                },
                hint: None,
            });
        }
        _ => {
            items.push(DoctorItem {
                category: category.to_string(),
                name: name.to_string(),
                status: missing_status,
                detail: "not found or unavailable".to_string(),
                hint: hint.map(str::to_string),
            });
        }
    }
}

fn check_optional_gate_tools(config: Option<&WardenConfig>, items: &mut Vec<DoctorItem>) {
    let optional_tools = [
        ("nextest", "cargo install cargo-nextest --locked"),
        ("machete", "cargo install cargo-machete"),
        ("deny", "cargo install cargo-deny --locked"),
        ("audit", "cargo install cargo-audit --locked"),
    ];
    for (tool, install_hint) in optional_tools {
        if tool_is_installed(tool) {
            items.push(DoctorItem {
                category: "gate-tools".to_string(),
                name: format!("cargo-{tool}"),
                status: DoctorStatus::Ok,
                detail: "installed".to_string(),
                hint: None,
            });
        } else {
            items.push(DoctorItem {
                category: "gate-tools".to_string(),
                name: format!("cargo-{tool}"),
                status: DoctorStatus::Info,
                detail: "not installed (optional)".to_string(),
                hint: Some(install_hint.to_string()),
            });
        }
    }

    if let Some(gate) = config.and_then(|c| c.gate.as_ref()) {
        for step in &gate.custom_steps {
            if let Some(tool) = &step.cargo_tool
                && !tool_is_installed(tool)
            {
                items.push(DoctorItem {
                    category: "gate-tools".to_string(),
                    name: format!("custom step `{}` (`cargo-{tool}`)", step.name),
                    status: DoctorStatus::Warn,
                    detail: format!("required cargo tool `cargo-{tool}` is not installed"),
                    hint: Some(format!("install `cargo-{tool}` for step `{}`", step.name)),
                });
            }
        }
    }
}

fn check_configured_paths(base_dir: &Path, config: &WardenConfig, items: &mut Vec<DoctorItem>) {
    if let Some(bl) = &config.baseline {
        let full = if bl.is_absolute() {
            bl.clone()
        } else {
            base_dir.join(bl)
        };
        let (status, detail, hint) = if full.is_file() {
            (
                DoctorStatus::Ok,
                format!("present ({})", bl.display()),
                None,
            )
        } else {
            (
                DoctorStatus::Warn,
                format!("configured baseline not found ({})", bl.display()),
                Some("run `cargo warden baseline generate`".to_string()),
            )
        };
        items.push(DoctorItem {
            category: "paths".to_string(),
            name: "baseline".to_string(),
            status,
            detail,
            hint,
        });
    }

    if let Some(ci) = &config.ci_parity {
        let exists = base_dir.join(&ci.workflow).is_file();
        items.push(DoctorItem {
            category: "paths".to_string(),
            name: "ci_parity.workflow".to_string(),
            status: if exists {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Warn
            },
            detail: if exists {
                format!("present ({})", ci.workflow.display())
            } else {
                format!("missing ({})", ci.workflow.display())
            },
            hint: (!exists).then(|| "update `[ci_parity] workflow` in warden.toml".to_string()),
        });
    }

    if let Some(dag) = &config.dag {
        for root in &dag.roots {
            let exists = base_dir.join(root).exists();
            items.push(DoctorItem {
                category: "paths".to_string(),
                name: format!("dag.root ({})", root.display()),
                status: if exists {
                    DoctorStatus::Ok
                } else {
                    DoctorStatus::Warn
                },
                detail: if exists {
                    "present".to_string()
                } else {
                    "directory does not exist".to_string()
                },
                hint: (!exists).then(|| "update `[dag] roots` in warden.toml".to_string()),
            });
        }
    }
}

fn check_git_hooks(base_dir: &Path, items: &mut Vec<DoctorItem>) {
    match hook::inspect_hooks(base_dir) {
        Ok(statuses) => {
            for st in statuses {
                let (status, detail, hint) = match st.state {
                    HookState::Managed {
                        mode,
                        executable: true,
                    } => (
                        DoctorStatus::Ok,
                        format!("managed ({})", mode.map_or("custom", |m| m.label())),
                        None,
                    ),
                    HookState::Managed {
                        mode,
                        executable: false,
                    } => (
                        DoctorStatus::Warn,
                        format!(
                            "managed ({}) but not executable",
                            mode.map_or("custom", |m| m.label())
                        ),
                        Some(format!("chmod +x {}", st.path.display())),
                    ),
                    HookState::Unmanaged { .. } => (
                        DoctorStatus::Info,
                        "custom unmanaged hook present".to_string(),
                        Some("use `cargo warden hook install --append` to integrate".to_string()),
                    ),
                    HookState::NotInstalled => (
                        DoctorStatus::Info,
                        "not installed".to_string(),
                        Some(format!(
                            "install with `cargo warden hook install --hook {}`",
                            st.kind.file_name()
                        )),
                    ),
                };
                items.push(DoctorItem {
                    category: "hooks".to_string(),
                    name: st.kind.file_name().to_string(),
                    status,
                    detail,
                    hint,
                });
            }
        }
        Err(_) => {
            items.push(DoctorItem {
                category: "hooks".to_string(),
                name: "git".to_string(),
                status: DoctorStatus::Info,
                detail: "not inside a Git repository".to_string(),
                hint: None,
            });
        }
    }
}

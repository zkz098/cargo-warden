//! Gate step definitions, built-in Cargo steps, custom steps, and `--fix` step builders.

use std::path::PathBuf;
use std::process::Command;

use super::impact::AffectedScope;
use crate::config::{CustomStepConfig, GateConfig};
use std::collections::BTreeSet;

/// A single built-in step in the gate pipeline.
pub struct Step {
    pub name: &'static str,
    pub program: &'static str,
    pub args: Vec<String>,
    pub tool: Option<&'static str>,
    pub env: Vec<(&'static str, &'static str)>,
}

impl Step {
    pub fn cargo(name: &'static str, args: &[&str]) -> Self {
        Self {
            name,
            program: "cargo",
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            tool: None,
            env: Vec::new(),
        }
    }

    pub fn cargo_tool(name: &'static str, tool: &'static str, args: &[&str]) -> Self {
        Self {
            tool: Some(tool),
            ..Self::cargo(name, args)
        }
    }

    pub fn rendered(&self) -> String {
        render_command(&self.env_pairs(), self.program, &self.args)
    }

    fn env_pairs(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }
}

/// Unified executable step supporting both built-in Cargo steps and user-defined custom steps.
#[derive(Debug, Clone)]
pub struct GateStep {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub tool: Option<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub read_only: bool,
}

impl GateStep {
    pub fn rendered(&self) -> String {
        render_command(&self.env, &self.program, &self.args)
    }
}

impl From<Step> for GateStep {
    fn from(step: Step) -> Self {
        let read_only = matches!(step.name, "fmt" | "machete" | "deny" | "audit");
        Self {
            name: step.name.to_string(),
            program: step.program.to_string(),
            args: step.args,
            tool: step.tool.map(str::to_string),
            env: step
                .env
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cwd: None,
            read_only,
        }
    }
}

fn render_command(env: &[(String, String)], program: &str, args: &[String]) -> String {
    let mut out = String::new();
    for (key, value) in env {
        out.push_str(&format!("{key}={value} "));
    }
    out.push_str(program);
    for arg in args {
        out.push(' ');
        out.push_str(arg);
    }
    out
}

pub fn tool_is_installed(tool: &str) -> bool {
    Command::new("cargo")
        .args([tool, "--version"])
        .output()
        .is_ok_and(|out| out.status.success())
}

pub fn toolchain_is_installed(toolchain: &str) -> bool {
    let Ok(output) = Command::new("rustup").args(["toolchain", "list"]).output() else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with(toolchain))
}

fn build_fmt_step(config: &GateConfig) -> Step {
    build_fmt_step_with_fix(config, false)
}

fn build_fmt_step_with_fix(config: &GateConfig, fix: bool) -> Step {
    let mut args = Vec::new();
    if let Some(pinned) = &config.rustfmt_toolchain
        && toolchain_is_installed(pinned)
    {
        args.push(format!("+{pinned}"));
    }
    args.push("fmt".to_string());
    args.push("--all".to_string());
    if !fix {
        args.push("--".to_string());
        args.push("--check".to_string());
    }

    Step {
        name: "fmt",
        program: "cargo",
        tool: None,
        env: Vec::new(),
        args,
    }
}

fn push_test_steps(steps: &mut Vec<Step>, config: &GateConfig) {
    push_test_steps_with_scope(steps, config, &AffectedScope::All);
}

fn push_test_steps_with_scope(steps: &mut Vec<Step>, config: &GateConfig, scope: &AffectedScope) {
    let affected_pkgs = match scope {
        AffectedScope::None { .. } => return,
        AffectedScope::All => None,
        AffectedScope::Packages { packages, .. } => Some(packages),
    };

    let has_nextest = tool_is_installed("nextest");
    if config.test_groups.is_empty() {
        let mut args = if has_nextest {
            vec!["nextest".to_string(), "run".to_string()]
        } else {
            vec!["test".to_string()]
        };
        match affected_pkgs {
            Some(pkgs) => push_package_flags(&mut args, pkgs),
            None if !has_nextest => args.push("--workspace".to_string()),
            None => {}
        }
        steps.push(Step {
            name: "test",
            program: "cargo",
            args,
            tool: if has_nextest { Some("nextest") } else { None },
            env: Vec::new(),
        });
        return;
    }

    for group in &config.test_groups {
        if let Some(step) = build_test_group_step(group, affected_pkgs, has_nextest) {
            steps.push(step);
        }
    }
}

fn build_test_group_step(
    group: &crate::config::TestGroupConfig,
    affected_pkgs: Option<&BTreeSet<String>>,
    has_nextest: bool,
) -> Option<Step> {
    let selected: Vec<String> = match affected_pkgs {
        Some(pkgs) => group
            .packages
            .iter()
            .filter(|p| pkgs.contains(*p))
            .cloned()
            .collect(),
        None => group.packages.clone(),
    };
    if selected.is_empty() {
        return None;
    }
    let mut args = if has_nextest {
        vec!["nextest".to_string(), "run".to_string()]
    } else {
        vec!["test".to_string()]
    };
    for pkg in &selected {
        args.push("-p".to_string());
        args.push(pkg.clone());
    }
    if !group.features.is_empty() {
        args.push("--features".to_string());
        args.push(group.features.join(","));
    }
    Some(Step {
        name: "test",
        program: "cargo",
        args,
        tool: if has_nextest { Some("nextest") } else { None },
        env: Vec::new(),
    })
}

fn push_package_flags(args: &mut Vec<String>, pkgs: &BTreeSet<String>) {
    for pkg in pkgs {
        args.push("-p".to_string());
        args.push(pkg.clone());
    }
}

fn replace_workspace_flag(args: Vec<String>, pkgs: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + pkgs.len() * 2);
    for arg in args {
        if arg == "--workspace" {
            push_package_flags(&mut out, pkgs);
        } else {
            out.push(arg);
        }
    }
    out
}

fn build_clippy_step(config: &GateConfig) -> Step {
    build_clippy_step_with_fix(config, false)
}

fn build_clippy_step_with_fix(config: &GateConfig, fix: bool) -> Step {
    let mut args = vec!["clippy".to_string()];
    if config.clippy_args.is_empty() {
        args.extend(["--workspace", "--all-targets"].map(str::to_string));
        if fix {
            args.extend(["--fix", "--allow-dirty", "--allow-staged"].map(str::to_string));
        } else {
            args.extend(["--", "-D", "warnings"].map(str::to_string));
        }
    } else if fix {
        let dash_pos = config.clippy_args.iter().position(|a| a == "--");
        let split_at = dash_pos.unwrap_or(config.clippy_args.len());
        args.extend(config.clippy_args[..split_at].iter().cloned());
        args.extend(["--fix", "--allow-dirty", "--allow-staged"].map(str::to_string));
        args.extend(config.clippy_args[split_at..].iter().cloned());
    } else {
        args.extend(config.clippy_args.iter().cloned());
    }

    Step {
        name: "clippy",
        program: "cargo",
        args,
        tool: None,
        env: Vec::new(),
    }
}

fn build_doc_step(config: &GateConfig) -> Step {
    let args = if config.doc_args.is_empty() {
        vec![
            "doc".to_string(),
            "--workspace".to_string(),
            "--no-deps".to_string(),
        ]
    } else {
        let mut custom = vec!["doc".to_string()];
        custom.extend(config.doc_args.iter().cloned());
        custom
    };

    Step {
        name: "doc",
        program: "cargo",
        args,
        tool: None,
        env: vec![("RUSTDOCFLAGS", "-D warnings")],
    }
}

fn build_full_audit_steps() -> Vec<Step> {
    vec![
        Step::cargo_tool("machete", "machete", &["machete", "--with-metadata"]),
        Step::cargo_tool(
            "deny",
            "deny",
            &[
                "deny", "check", "-W", "rejected", "bans", "licenses", "sources",
            ],
        ),
        Step::cargo_tool("audit", "audit", &["audit"]),
    ]
}

pub fn build_steps(config: &GateConfig, full: bool) -> Vec<Step> {
    let mut steps = vec![build_fmt_step(config), build_clippy_step(config)];

    push_test_steps(&mut steps, config);
    steps.push(build_doc_step(config));

    if full {
        steps.extend(build_full_audit_steps());
    }

    steps
}

/// Filter `build_steps` by `--only` and `--skip` (plus `config.skip_steps`).
pub fn build_filtered_steps(
    config: &GateConfig,
    full: bool,
    only: &[String],
    skip: &[String],
) -> Vec<Step> {
    build_steps(config, full)
        .into_iter()
        .filter(|step| step_selected(step.name, config, only, skip))
        .collect()
}

fn step_selected(name: &str, config: &GateConfig, only: &[String], skip: &[String]) -> bool {
    let in_only = only.is_empty() || only.iter().any(|s| s == name);
    let is_skipped = config.skip_steps.iter().any(|s| s == name) || skip.iter().any(|s| s == name);
    in_only && !is_skipped
}

fn parse_command_shorthand(cmd: &str) -> (String, Vec<String>) {
    let trimmed = cmd.trim();
    let needs_shell = ["&&", "||", "|", ";", ">", "<", "$"]
        .iter()
        .any(|tok| trimmed.contains(tok));
    if needs_shell {
        if cfg!(windows) {
            return (
                "cmd".to_string(),
                vec!["/C".to_string(), trimmed.to_string()],
            );
        }
        return (
            "sh".to_string(),
            vec!["-c".to_string(), trimmed.to_string()],
        );
    }
    let mut parts = trimmed.split_whitespace();
    let program = parts.next().unwrap_or("").to_string();
    let args = parts.map(str::to_string).collect();
    (program, args)
}

fn custom_to_gate_step(custom: &CustomStepConfig, fix: bool) -> GateStep {
    let has_fix_override = fix && (custom.fix_args.is_some() || custom.fix_command.is_some());
    let (program, args) = if fix && let Some(fix_cmd) = &custom.fix_command {
        parse_command_shorthand(fix_cmd)
    } else if !custom.program.trim().is_empty() {
        let args = if fix && let Some(fix_args) = &custom.fix_args {
            fix_args.clone()
        } else {
            custom.args.clone()
        };
        (custom.program.clone(), args)
    } else if let Some(cmd) = &custom.command {
        parse_command_shorthand(cmd)
    } else {
        (String::new(), Vec::new())
    };

    GateStep {
        name: custom.name.clone(),
        program,
        args,
        tool: custom.cargo_tool.clone(),
        env: custom
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        cwd: custom.cwd.clone(),
        read_only: custom.read_only && !has_fix_override,
    }
}

/// Build all built-in and custom gate steps (supporting `--fix` ordering and overrides).
pub fn build_all_steps(config: &GateConfig, full: bool, fix: bool) -> Vec<GateStep> {
    build_all_steps_with_scope(config, full, fix, &AffectedScope::All)
}

/// Build all built-in and custom gate steps scoped to `scope` for incremental runs.
pub fn build_all_steps_with_scope(
    config: &GateConfig,
    full: bool,
    fix: bool,
    scope: &AffectedScope,
) -> Vec<GateStep> {
    let mut steps = build_builtin_steps_with_scope(config, full, fix, scope);

    for custom in &config.custom_steps {
        if custom.full_only && !full {
            continue;
        }
        if custom.name.trim().is_empty() {
            continue;
        }
        steps.push(custom_to_gate_step(custom, fix));
    }

    steps
}

fn scope_cargo_step(mut step: Step, scope: &AffectedScope) -> Option<Step> {
    match scope {
        AffectedScope::None { .. } => None,
        AffectedScope::All => Some(step),
        AffectedScope::Packages { packages, .. } => {
            step.args = replace_workspace_flag(step.args, packages);
            Some(step)
        }
    }
}

fn build_builtin_steps_with_scope(
    config: &GateConfig,
    full: bool,
    fix: bool,
    scope: &AffectedScope,
) -> Vec<GateStep> {
    let include_fmt = !matches!(
        scope,
        AffectedScope::None {
            has_rust_changes: false
        }
    );
    let mut steps = Vec::new();

    if fix {
        // In --fix mode, run clippy --fix before rustfmt so any auto-fixed code gets formatted.
        if let Some(clippy) = scope_cargo_step(build_clippy_step_with_fix(config, true), scope) {
            steps.push(GateStep {
                read_only: false,
                ..GateStep::from(clippy)
            });
        }
        if include_fmt {
            steps.push(GateStep {
                read_only: false,
                ..GateStep::from(build_fmt_step_with_fix(config, true))
            });
        }
    } else {
        if include_fmt {
            steps.push(GateStep::from(build_fmt_step(config)));
        }
        if let Some(clippy) = scope_cargo_step(build_clippy_step(config), scope) {
            steps.push(GateStep::from(clippy));
        }
    }

    let mut test_steps = Vec::new();
    push_test_steps_with_scope(&mut test_steps, config, scope);
    steps.extend(test_steps.into_iter().map(GateStep::from));
    if let Some(doc) = scope_cargo_step(build_doc_step(config), scope) {
        steps.push(GateStep::from(doc));
    }
    if full {
        steps.extend(build_full_audit_steps().into_iter().map(GateStep::from));
    }
    steps
}

/// Filter `build_all_steps` by `--only`, `--skip`, and `config.skip_steps`.
pub fn build_filtered_all_steps(
    config: &GateConfig,
    full: bool,
    fix: bool,
    only: &[String],
    skip: &[String],
) -> Vec<GateStep> {
    build_filtered_all_steps_with_scope(config, full, fix, only, skip, &AffectedScope::All)
}

/// Filter `build_all_steps_with_scope` by `--only`, `--skip`, and `config.skip_steps`.
pub fn build_filtered_all_steps_with_scope(
    config: &GateConfig,
    full: bool,
    fix: bool,
    only: &[String],
    skip: &[String],
    scope: &AffectedScope,
) -> Vec<GateStep> {
    build_all_steps_with_scope(config, full, fix, scope)
        .into_iter()
        .filter(|step| step_selected(&step.name, config, only, skip))
        .collect()
}

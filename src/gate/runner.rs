//! Gate execution engine.
//!
//! Mirrors full CI verification passes locally with toolchain pinning,
//! cargo metadata environment scrubbing, and granular step reporting.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::checks;
use crate::config::{GateConfig, WardenConfig};

/// A single step in the gate pipeline.
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
        let mut out = String::new();
        for (key, value) in &self.env {
            out.push_str(&format!("{key}={value} "));
        }
        out.push_str(self.program);
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

/// Scrub `CARGO_PKG_*` and other Cargo environment variables before invoking child cargo tools.
pub fn scrub_cargo_metadata(command: &mut Command) {
    const EXTRA: &[&str] = &["CARGO_MANIFEST_DIR", "CARGO_CRATE_NAME", "OUT_DIR"];
    for (key, _) in std::env::vars() {
        if key.starts_with("CARGO_PKG_") || EXTRA.contains(&key.as_str()) {
            tracing::trace!(key = %key, "scrubbed cargo environment variable");
            command.env_remove(key);
        }
    }
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
    if let Some(pinned) = &config.rustfmt_toolchain
        && toolchain_is_installed(pinned)
    {
        return Step {
            name: "fmt",
            program: "cargo",
            tool: None,
            env: Vec::new(),
            args: vec![
                format!("+{pinned}"),
                "fmt".to_string(),
                "--all".to_string(),
                "--".to_string(),
                "--check".to_string(),
            ],
        };
    }

    Step::cargo("fmt", &["fmt", "--all", "--", "--check"])
}

fn push_test_steps(steps: &mut Vec<Step>, config: &GateConfig) {
    let has_nextest = tool_is_installed("nextest");
    if config.test_groups.is_empty() {
        let step = if has_nextest {
            Step::cargo_tool("test", "nextest", &["nextest", "run"])
        } else {
            Step::cargo("test", &["test", "--workspace"])
        };
        steps.push(step);
        return;
    }

    for group in &config.test_groups {
        let mut args = if has_nextest {
            vec!["nextest".to_string(), "run".to_string()]
        } else {
            vec!["test".to_string()]
        };
        for pkg in &group.packages {
            args.push("-p".to_string());
            args.push(pkg.clone());
        }
        if !group.features.is_empty() {
            args.push("--features".to_string());
            args.push(group.features.join(","));
        }
        steps.push(Step {
            name: "test",
            program: "cargo",
            args,
            tool: if has_nextest { Some("nextest") } else { None },
            env: Vec::new(),
        });
    }
}

/// Options controlling a `cargo warden gate` execution.
#[derive(Debug, Clone, Default)]
pub struct GateRunOptions {
    pub full: bool,
    pub keep_going: bool,
    pub skip_checks: bool,
    pub only: Vec<String>,
    pub skip: Vec<String>,
}

fn build_clippy_step(config: &GateConfig) -> Step {
    if config.clippy_args.is_empty() {
        return Step::cargo(
            "clippy",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        );
    }

    let mut args = vec!["clippy".to_string()];
    args.extend(config.clippy_args.iter().cloned());
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

pub fn build_steps(config: &GateConfig, full: bool) -> Vec<Step> {
    let mut steps = vec![build_fmt_step(config), build_clippy_step(config)];

    push_test_steps(&mut steps, config);
    steps.push(build_doc_step(config));

    if full {
        steps.push(Step::cargo_tool(
            "machete",
            "machete",
            &["machete", "--with-metadata"],
        ));
        steps.push(Step::cargo_tool(
            "deny",
            "deny",
            &[
                "deny", "check", "-W", "rejected", "bans", "licenses", "sources",
            ],
        ));
        steps.push(Step::cargo_tool("audit", "audit", &["audit"]));
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
        .filter(|step| {
            let in_only = only.is_empty() || only.iter().any(|s| s == step.name);
            let is_skipped = config.skip_steps.iter().any(|s| s == step.name)
                || skip.iter().any(|s| s == step.name);
            in_only && !is_skipped
        })
        .collect()
}

/// Run static checks (when enabled) followed by the gate's Cargo toolchain steps.
pub fn run_gate_with_checks(
    root: &Path,
    config: &WardenConfig,
    opts: &GateRunOptions,
) -> Result<()> {
    let gate_cfg = config.gate.clone().unwrap_or_default();

    let run_checks = !opts.skip_checks
        && gate_cfg.include_checks
        && !opts.skip.iter().any(|s| s == "checks" || s == "check")
        && !gate_cfg
            .skip_steps
            .iter()
            .any(|s| s == "checks" || s == "check")
        && (opts.only.is_empty() || opts.only.iter().any(|s| s == "checks" || s == "check"));

    if run_checks {
        checks::run_static_checks(config, root)?;
        println!();
    }

    let steps = build_filtered_steps(&gate_cfg, opts.full, &opts.only, &opts.skip);
    if steps.is_empty() {
        if run_checks {
            return Ok(());
        }
        bail!("No gate steps selected to run");
    }

    run_steps(
        root,
        &steps,
        gate_cfg.rustdocflags.as_deref(),
        opts.full,
        opts.keep_going,
    )
}

/// Run Cargo toolchain steps configured for `cargo warden check` (e.g. `fmt`, `clippy`, `doc`).
pub fn run_check_toolchain_steps(root: &Path, config: &WardenConfig, full: bool) -> Result<()> {
    let selected: Vec<String> = if full {
        vec!["fmt".to_string(), "clippy".to_string(), "doc".to_string()]
    } else if let Some(chk) = &config.check {
        let mut names = Vec::new();
        if chk.fmt {
            names.push("fmt".to_string());
        }
        if chk.clippy {
            names.push("clippy".to_string());
        }
        if chk.doc {
            names.push("doc".to_string());
        }
        names
    } else {
        Vec::new()
    };

    if selected.is_empty() {
        return Ok(());
    }

    let gate_cfg = config.gate.clone().unwrap_or_default();
    let steps = build_filtered_steps(&gate_cfg, false, &selected, &[]);
    println!();
    run_steps(root, &steps, gate_cfg.rustdocflags.as_deref(), false, false)
}

pub fn run_gate(root: &Path, config: &GateConfig, full: bool, keep_going: bool) -> Result<()> {
    let steps = build_filtered_steps(config, full, &[], &[]);
    run_steps(
        root,
        &steps,
        config.rustdocflags.as_deref(),
        full,
        keep_going,
    )
}

fn run_steps(
    root: &Path,
    steps: &[Step],
    custom_rustdocflags: Option<&str>,
    full: bool,
    keep_going: bool,
) -> Result<()> {
    tracing::debug!(
        step_count = steps.len(),
        full,
        keep_going,
        "starting gate pipeline execution"
    );
    println!(
        "[gate] running {} step(s) ({} profile)",
        steps.len(),
        if full { "full" } else { "quick" }
    );

    let mut failures: Vec<(&'static str, String, u128)> = Vec::new();
    let mut skipped: Vec<(&'static str, &'static str)> = Vec::new();
    let mut passed = 0usize;
    let started = Instant::now();

    for step in steps {
        if let Some(tool) = step.tool
            && !tool_is_installed(tool)
        {
            tracing::warn!(
                step = step.name,
                tool = tool,
                "tool is not installed; skipping step"
            );
            println!(
                "\n==> {}\n    skipped: cargo-{tool} is not installed (run `cargo install cargo-{tool} --locked`)",
                step.name
            );
            skipped.push((step.name, tool));
            continue;
        }

        println!("\n==> {}\n    $ {}", step.name, step.rendered());
        tracing::debug!(step = step.name, command = %step.rendered(), "spawning step process");
        let step_started = Instant::now();
        let mut command = Command::new(step.program);
        command
            .args(&step.args)
            .current_dir(root)
            .env("CARGO_BUILD_WARNINGS", "deny")
            .env("CARGO_REGISTRIES_CRATES_IO_PROTOCOL", "sparse")
            .envs(step.env.iter().copied());
        if step.name == "doc"
            && let Some(flags) = custom_rustdocflags
        {
            command.env("RUSTDOCFLAGS", flags);
        }

        scrub_cargo_metadata(&mut command);

        let status = command
            .status()
            .with_context(|| format!("Failed to spawn `{}`", step.rendered()))?;

        let elapsed = step_started.elapsed().as_millis();
        tracing::trace!(
            step = step.name,
            elapsed_ms = elapsed,
            success = status.success(),
            "step process completed"
        );
        if status.success() {
            passed += 1;
            println!("<== {} ok ({} ms)", step.name, elapsed);
            continue;
        }

        failures.push((step.name, step.rendered(), elapsed));
        println!("<== {} FAILED ({} ms)", step.name, elapsed);
        if !keep_going {
            break;
        }
    }

    println!("\n──────── Gate Summary ────────");
    println!(
        "{} passed, {} failed, {} skipped, in {} s",
        passed,
        failures.len(),
        skipped.len(),
        started.elapsed().as_secs()
    );

    if failures.is_empty() {
        println!("[gate] ALL GREEN ✨");
        Ok(())
    } else {
        for (name, command, elapsed) in &failures {
            println!("  ❌ {name} ({elapsed} ms): $ {command}");
        }
        bail!("{} gate step(s) failed", failures.len())
    }
}

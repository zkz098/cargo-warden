//! Gate execution engine.
//!
//! Mirrors full CI verification passes locally with toolchain pinning,
//! cargo metadata environment scrubbing, custom steps, parallel read-only
//! execution, `--fix` mode, and granular step reporting.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::checks;
use crate::config::{GateConfig, WardenConfig};

pub use super::steps::{
    GateStep, Step, build_all_steps, build_filtered_all_steps, build_filtered_steps, build_steps,
    tool_is_installed, toolchain_is_installed,
};

/// Options controlling a `cargo warden gate` execution.
#[derive(Debug, Clone, Default)]
pub struct GateRunOptions {
    pub full: bool,
    pub keep_going: bool,
    pub skip_checks: bool,
    pub fix: bool,
    pub parallel: Option<bool>,
    pub only: Vec<String>,
    pub skip: Vec<String>,
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

/// Run static checks (when enabled) followed by the gate's Cargo and custom steps.
pub fn run_gate_with_checks(
    root: &Path,
    config: &WardenConfig,
    opts: &GateRunOptions,
) -> Result<()> {
    run_gate_with_static_options(root, config, opts, &checks::StaticRunOptions::default())
}

/// Run static checks with explicit `StaticRunOptions` followed by the gate's Cargo and custom steps.
pub fn run_gate_with_static_options(
    root: &Path,
    config: &WardenConfig,
    opts: &GateRunOptions,
    static_opts: &checks::StaticRunOptions<'_>,
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
        if opts.fix {
            checks::run_static_fixes(config, root, !static_opts.format.is_structured())?;
        }
        checks::run_static_checks_with_options(config, root, static_opts)?;
        if !static_opts.format.is_structured() {
            println!();
        }
    }

    let steps = build_filtered_all_steps(&gate_cfg, opts.full, opts.fix, &opts.only, &opts.skip);
    if steps.is_empty() {
        if run_checks {
            return Ok(());
        }
        bail!("No gate steps selected to run");
    }

    let parallel = opts.parallel.unwrap_or(gate_cfg.parallel) && !opts.fix;
    run_gate_steps(
        root,
        &steps,
        gate_cfg.rustdocflags.as_deref(),
        opts.full,
        opts.keep_going,
        parallel,
    )
}

/// Run Cargo toolchain steps configured for `cargo warden check` (e.g. `fmt`, `clippy`, `doc`).
pub fn run_check_toolchain_steps(root: &Path, config: &WardenConfig, full: bool) -> Result<()> {
    run_check_toolchain_steps_with_options(root, config, full, false, None)
}

/// Run Cargo toolchain steps configured for `cargo warden check` with `--fix` and parallel control.
pub fn run_check_toolchain_steps_with_options(
    root: &Path,
    config: &WardenConfig,
    full: bool,
    fix: bool,
    parallel_override: Option<bool>,
) -> Result<()> {
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
    let steps = build_filtered_all_steps(&gate_cfg, false, fix, &selected, &[]);
    let parallel = parallel_override.unwrap_or(gate_cfg.parallel) && !fix;
    println!();
    run_gate_steps(
        root,
        &steps,
        gate_cfg.rustdocflags.as_deref(),
        false,
        false,
        parallel,
    )
}

pub fn run_gate(root: &Path, config: &GateConfig, full: bool, keep_going: bool) -> Result<()> {
    let steps = build_filtered_all_steps(config, full, false, &[], &[]);
    run_gate_steps(
        root,
        &steps,
        config.rustdocflags.as_deref(),
        full,
        keep_going,
        config.parallel,
    )
}

fn prepare_command(
    root: &Path,
    step: &GateStep,
    custom_rustdocflags: Option<&str>,
) -> Result<Command> {
    if step.program.trim().is_empty() {
        bail!("Custom gate step `{}` has no program or command", step.name);
    }
    let work_dir = match &step.cwd {
        Some(rel) if rel.is_absolute() => rel.clone(),
        Some(rel) => root.join(rel),
        None => root.to_path_buf(),
    };

    let mut command = Command::new(&step.program);
    command
        .args(&step.args)
        .current_dir(work_dir)
        .env("CARGO_BUILD_WARNINGS", "deny")
        .env("CARGO_REGISTRIES_CRATES_IO_PROTOCOL", "sparse");
    for (k, v) in &step.env {
        command.env(k, v);
    }
    if step.name == "doc"
        && let Some(flags) = custom_rustdocflags
    {
        command.env("RUSTDOCFLAGS", flags);
    }

    scrub_cargo_metadata(&mut command);
    Ok(command)
}

enum StepRunOutcome {
    Skipped(String),
    Passed {
        elapsed_ms: u128,
        output: Option<Vec<u8>>,
    },
    Failed {
        elapsed_ms: u128,
        output: Option<Vec<u8>>,
    },
}

fn run_single_step(
    root: &Path,
    step: &GateStep,
    custom_rustdocflags: Option<&str>,
    capture_output: bool,
) -> Result<StepRunOutcome> {
    if let Some(tool) = &step.tool
        && !tool_is_installed(tool)
    {
        tracing::warn!(step = %step.name, tool = %tool, "tool is not installed; skipping step");
        return Ok(StepRunOutcome::Skipped(tool.clone()));
    }

    let step_started = Instant::now();
    let mut command = prepare_command(root, step, custom_rustdocflags)?;

    if capture_output {
        let out = command
            .output()
            .with_context(|| format!("Failed to spawn `{}`", step.rendered()))?;
        let elapsed_ms = step_started.elapsed().as_millis();
        let mut combined = out.stdout;
        combined.extend_from_slice(&out.stderr);
        if out.status.success() {
            Ok(StepRunOutcome::Passed {
                elapsed_ms,
                output: Some(combined),
            })
        } else {
            Ok(StepRunOutcome::Failed {
                elapsed_ms,
                output: Some(combined),
            })
        }
    } else {
        println!("\n==> {}\n    $ {}", step.name, step.rendered());
        let status = command
            .status()
            .with_context(|| format!("Failed to spawn `{}`", step.rendered()))?;
        let elapsed_ms = step_started.elapsed().as_millis();
        if status.success() {
            Ok(StepRunOutcome::Passed {
                elapsed_ms,
                output: None,
            })
        } else {
            Ok(StepRunOutcome::Failed {
                elapsed_ms,
                output: None,
            })
        }
    }
}

fn partition_step_batches(steps: &[GateStep], parallel: bool) -> Vec<&[GateStep]> {
    if !parallel {
        return steps.chunks(1).collect();
    }
    let mut batches = Vec::new();
    let mut i = 0;
    while i < steps.len() {
        if !steps[i].read_only {
            batches.push(&steps[i..=i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < steps.len() && steps[i].read_only {
            i += 1;
        }
        batches.push(&steps[start..i]);
    }
    batches
}

struct GateTracker {
    passed: usize,
    failures: Vec<(String, String, u128)>,
    skipped: Vec<(String, String)>,
}

impl GateTracker {
    fn new() -> Self {
        Self {
            passed: 0,
            failures: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn record_outcome(&mut self, step: &GateStep, outcome: StepRunOutcome) -> bool {
        match outcome {
            StepRunOutcome::Skipped(tool) => {
                println!(
                    "\n==> {}\n    skipped: cargo-{tool} is not installed (run `cargo install cargo-{tool} --locked`)",
                    step.name
                );
                self.skipped.push((step.name.clone(), tool));
                false
            }
            StepRunOutcome::Passed { elapsed_ms, output } => {
                print_captured_output(step, output.as_deref());
                self.passed += 1;
                println!("<== {} ok ({} ms)", step.name, elapsed_ms);
                false
            }
            StepRunOutcome::Failed { elapsed_ms, output } => {
                print_captured_output(step, output.as_deref());
                self.failures
                    .push((step.name.clone(), step.rendered(), elapsed_ms));
                println!("<== {} FAILED ({} ms)", step.name, elapsed_ms);
                true
            }
        }
    }
}

fn print_captured_output(step: &GateStep, output: Option<&[u8]>) {
    if let Some(bytes) = output {
        println!("\n==> {}\n    $ {}", step.name, step.rendered());
        let _ = std::io::stdout().write_all(bytes);
    }
}

fn execute_batch(
    root: &Path,
    batch: &[GateStep],
    custom_rustdocflags: Option<&str>,
) -> Result<Vec<StepRunOutcome>> {
    if batch.len() == 1 {
        return Ok(vec![run_single_step(
            root,
            &batch[0],
            custom_rustdocflags,
            false,
        )?]);
    }

    println!(
        "\n[gate] running {} read-only step(s) in parallel: {}",
        batch.len(),
        batch
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let results: Vec<Result<StepRunOutcome>> = std::thread::scope(|s| {
        let handles: Vec<_> = batch
            .iter()
            .map(|step| s.spawn(|| run_single_step(root, step, custom_rustdocflags, true)))
            .collect();
        handles
            .into_iter()
            .map(|h| match h.join() {
                Ok(res) => res,
                Err(_) => bail!("Gate worker thread panicked"),
            })
            .collect()
    });
    results.into_iter().collect()
}

fn run_gate_steps(
    root: &Path,
    steps: &[GateStep],
    custom_rustdocflags: Option<&str>,
    full: bool,
    keep_going: bool,
    parallel: bool,
) -> Result<()> {
    tracing::debug!(
        step_count = steps.len(),
        full,
        keep_going,
        parallel,
        "starting gate pipeline execution"
    );
    println!(
        "[gate] running {} step(s) ({} profile{})",
        steps.len(),
        if full { "full" } else { "quick" },
        if parallel { ", parallel read-only" } else { "" }
    );

    let mut tracker = GateTracker::new();
    let started = Instant::now();

    for batch in partition_step_batches(steps, parallel) {
        let outcomes = execute_batch(root, batch, custom_rustdocflags)?;
        let mut batch_failed = false;
        for (step, outcome) in batch.iter().zip(outcomes) {
            if tracker.record_outcome(step, outcome) {
                batch_failed = true;
            }
        }
        if batch_failed && !keep_going {
            break;
        }
    }

    println!("\n──────── Gate Summary ────────");
    println!(
        "{} passed, {} failed, {} skipped, in {} s",
        tracker.passed,
        tracker.failures.len(),
        tracker.skipped.len(),
        started.elapsed().as_secs()
    );

    if tracker.failures.is_empty() {
        println!("[gate] ALL GREEN ✨");
        Ok(())
    } else {
        for (name, command, elapsed) in &tracker.failures {
            println!("  ❌ {name} ({elapsed} ms): $ {command}");
        }
        bail!("{} gate step(s) failed", tracker.failures.len())
    }
}

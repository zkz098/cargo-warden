//! Gate execution engine.
//!
//! Mirrors full CI verification passes locally with toolchain pinning,
//! cargo metadata environment scrubbing, and granular step reporting.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::config::GateConfig;

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
            command.env_remove(key);
        }
    }
}

pub fn tool_is_installed(tool: &str) -> bool {
    Command::new(format!("cargo-{tool}"))
        .arg("--version")
        .output()
        .is_ok()
}

pub fn toolchain_is_installed(toolchain: &str) -> bool {
    let Ok(output) = Command::new("rustup").args(["toolchain", "list"]).output() else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with(toolchain))
}

pub fn build_steps(config: &GateConfig, full: bool) -> Vec<Step> {
    let mut steps = Vec::new();

    // 1. Rustfmt step
    if let Some(pinned) = &config.rustfmt_toolchain {
        if toolchain_is_installed(pinned) {
            steps.push(Step {
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
            });
        } else {
            steps.push(Step::cargo("fmt", &["fmt", "--all", "--", "--check"]));
        }
    } else {
        steps.push(Step::cargo("fmt", &["fmt", "--all", "--", "--check"]));
    }

    // 2. Clippy step
    steps.push(Step::cargo(
        "clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    ));

    // 3. Tests / nextest
    let has_nextest = tool_is_installed("nextest");
    if config.test_groups.is_empty() {
        if has_nextest {
            steps.push(Step::cargo_tool("test", "nextest", &["nextest", "run"]));
        } else {
            steps.push(Step::cargo("test", &["test", "--workspace"]));
        }
    } else {
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

    // 4. Docs step
    let mut doc_step = Step::cargo("doc", &["doc", "--workspace", "--no-deps"]);
    doc_step.env.push(("RUSTDOCFLAGS", "-D warnings"));
    steps.push(doc_step);

    // 5. Full profile steps
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

pub fn run_gate(root: &Path, config: &GateConfig, full: bool, keep_going: bool) -> Result<()> {
    let steps = build_steps(config, full);
    println!(
        "[gate] running {} step(s) ({} profile)",
        steps.len(),
        if full { "full" } else { "quick" }
    );

    let mut failures: Vec<(&'static str, String, u128)> = Vec::new();
    let mut skipped: Vec<(&'static str, &'static str)> = Vec::new();
    let mut passed = 0usize;
    let started = Instant::now();

    for step in &steps {
        if let Some(tool) = step.tool
            && !tool_is_installed(tool)
        {
            println!(
                "\n==> {}\n    skipped: cargo-{tool} is not installed (run `cargo install cargo-{tool} --locked`)",
                step.name
            );
            skipped.push((step.name, tool));
            continue;
        }

        println!("\n==> {}\n    $ {}", step.name, step.rendered());
        let step_started = Instant::now();
        let mut command = Command::new(step.program);
        command
            .args(&step.args)
            .current_dir(root)
            .env("CARGO_BUILD_WARNINGS", "deny")
            .env("CARGO_REGISTRIES_CRATES_IO_PROTOCOL", "sparse")
            .envs(step.env.iter().copied());

        scrub_cargo_metadata(&mut command);

        let status = command
            .status()
            .with_context(|| format!("Failed to spawn `{}`", step.rendered()))?;

        let elapsed = step_started.elapsed().as_millis();
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

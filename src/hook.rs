//! Git hook management (`pre-commit`, `pre-push`) for `cargo-warden`.
//!
//! Safely installs, updates, inspects, and uninstalls shell hooks in the
//! repository's resolved Git hooks directory (`git rev-parse --git-path hooks`)
//! using delimited marker blocks so existing custom hooks are preserved.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MARKER_START: &str = "# >>> cargo-warden >>>";
const MARKER_END: &str = "# <<< cargo-warden <<<";

/// Supported Git hook lifecycle stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    PreCommit,
    PrePush,
}

impl HookKind {
    /// All hook types inspected by `hook status`.
    pub const ALL: [HookKind; 2] = [HookKind::PreCommit, HookKind::PrePush];

    /// Git hook filename under the hooks directory.
    pub fn file_name(self) -> &'static str {
        match self {
            HookKind::PreCommit => "pre-commit",
            HookKind::PrePush => "pre-push",
        }
    }
}

/// Command profile executed by the installed Git hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookMode {
    /// Fast static checks (`cargo warden check`).
    Check,
    /// Incremental static checks on staged files only (`cargo warden check --staged`).
    CheckStaged,
    /// Quick local CI gate (`cargo warden gate`).
    Gate,
    /// Full local CI gate (`cargo warden gate --full`).
    GateFull,
}

impl HookMode {
    /// Subcommand arguments passed to `cargo-warden` / `cargo run`.
    pub fn subcommand_args(self) -> &'static str {
        match self {
            HookMode::Check => "check",
            HookMode::CheckStaged => "check --staged",
            HookMode::Gate => "gate",
            HookMode::GateFull => "gate --full",
        }
    }

    /// Human-readable label for status output.
    pub fn label(self) -> &'static str {
        self.subcommand_args()
    }
}

/// Strategy when a target hook file already exists without cargo-warden markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingHookPolicy {
    /// Refuse to overwrite or modify an unmanaged existing hook.
    Fail,
    /// Replace the existing hook file completely.
    Force,
    /// Append the managed cargo-warden block to the existing hook file.
    Append,
}

/// Outcome of an install operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Created(PathBuf),
    Updated(PathBuf),
    Appended(PathBuf),
    Overwritten(PathBuf),
}

/// Outcome of an uninstall operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    RemovedFile(PathBuf),
    RemovedBlock(PathBuf),
    NotInstalled(PathBuf),
}

/// Inspection state for a single Git hook slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookState {
    NotInstalled,
    Managed {
        mode: Option<HookMode>,
        executable: bool,
    },
    Unmanaged {
        executable: bool,
    },
}

/// Status record returned for each hook in `HookKind::ALL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookStatus {
    pub kind: HookKind,
    pub path: PathBuf,
    pub state: HookState,
}

/// Resolve the Git hooks directory for `root` using `git rev-parse --git-path hooks`.
///
/// This handles standard repositories, Git worktrees, submodules, and custom
/// `core.hooksPath` configurations.
pub fn resolve_hooks_dir(root: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "hooks"])
        .output()
        .context("Failed to execute `git rev-parse --git-path hooks`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Not a Git repository (or git failed) at {}: {}",
            root.display(),
            stderr.trim()
        );
    }

    let raw = String::from_utf8(output.stdout)
        .context("Git returned non-UTF-8 hooks path")?
        .trim()
        .to_string();

    let parsed = PathBuf::from(raw);
    if parsed.is_absolute() {
        Ok(parsed)
    } else {
        Ok(root.join(parsed))
    }
}

/// Render the delimited `cargo-warden` shell block for the given mode.
pub fn render_managed_block(mode: HookMode) -> String {
    let args = mode.subcommand_args();
    format!(
        "{MARKER_START}\n\
         if command -v cargo-warden >/dev/null 2>&1; then\n\
         \tcargo-warden {args} || exit $?\n\
         elif [ -x \"${{CARGO_HOME:-$HOME/.cargo}}/bin/cargo-warden\" ]; then\n\
         \t\"${{CARGO_HOME:-$HOME/.cargo}}/bin/cargo-warden\" {args} || exit $?\n\
         elif [ -f Cargo.toml ] && grep -q '^name = \"cargo-warden\"' Cargo.toml 2>/dev/null; then\n\
         \tcargo run --quiet -- {args} || exit $?\n\
         else\n\
         \techo \"[cargo-warden] binary not found in PATH; install with 'cargo install cargo-warden' or bypass with '--no-verify'\" >&2\n\
         \texit 1\n\
         fi\n\
         {MARKER_END}"
    )
}

fn render_standalone_script(mode: HookMode) -> String {
    format!(
        "#!/usr/bin/env sh\nset -eu\n\n{}\n",
        render_managed_block(mode)
    )
}

/// Install or update a `cargo-warden` Git hook under `root`.
pub fn install_hook(
    root: &Path,
    kind: HookKind,
    mode: HookMode,
    policy: ExistingHookPolicy,
) -> Result<InstallOutcome> {
    let hooks_dir = resolve_hooks_dir(root)?;
    fs::create_dir_all(&hooks_dir).with_context(|| {
        format!(
            "Failed to create Git hooks directory {}",
            hooks_dir.display()
        )
    })?;

    let hook_path = hooks_dir.join(kind.file_name());
    tracing::debug!(
        hook = kind.file_name(),
        path = %hook_path.display(),
        mode = mode.label(),
        "installing git hook"
    );

    if !hook_path.exists() {
        write_hook_file(&hook_path, &render_standalone_script(mode))?;
        return Ok(InstallOutcome::Created(hook_path));
    }

    let existing = fs::read_to_string(&hook_path)
        .with_context(|| format!("Failed to read existing hook {}", hook_path.display()))?;

    if let Some((before, after)) = split_managed_block(&existing) {
        if policy == ExistingHookPolicy::Force {
            write_hook_file(&hook_path, &render_standalone_script(mode))?;
            return Ok(InstallOutcome::Overwritten(hook_path));
        }

        let updated = rebuild_with_block(before, &render_managed_block(mode), after);
        write_hook_file(&hook_path, &updated)?;
        return Ok(InstallOutcome::Updated(hook_path));
    }

    match policy {
        ExistingHookPolicy::Fail => {
            bail!(
                "Existing non-cargo-warden hook found at {}. Re-run with `--append` to preserve it or `--force` to overwrite it.",
                hook_path.display()
            );
        }
        ExistingHookPolicy::Force => {
            write_hook_file(&hook_path, &render_standalone_script(mode))?;
            Ok(InstallOutcome::Overwritten(hook_path))
        }
        ExistingHookPolicy::Append => {
            let mut combined = existing;
            if !combined.ends_with('\n') && !combined.is_empty() {
                combined.push('\n');
            }
            if !combined.is_empty() && !combined.ends_with("\n\n") {
                combined.push('\n');
            }
            combined.push_str(&render_managed_block(mode));
            combined.push('\n');
            write_hook_file(&hook_path, &combined)?;
            Ok(InstallOutcome::Appended(hook_path))
        }
    }
}

/// Uninstall a `cargo-warden` managed Git hook (or remove its block) under `root`.
pub fn uninstall_hook(root: &Path, kind: HookKind) -> Result<UninstallOutcome> {
    let hooks_dir = resolve_hooks_dir(root)?;
    let hook_path = hooks_dir.join(kind.file_name());

    if !hook_path.exists() {
        return Ok(UninstallOutcome::NotInstalled(hook_path));
    }

    let existing = fs::read_to_string(&hook_path)
        .with_context(|| format!("Failed to read hook {}", hook_path.display()))?;

    let Some((before, after)) = split_managed_block(&existing) else {
        bail!(
            "Hook {} is not managed by cargo-warden; refusing to remove it.",
            hook_path.display()
        );
    };

    let remaining = merge_remaining(before, after);
    if is_empty_shell_stub(&remaining) {
        fs::remove_file(&hook_path)
            .with_context(|| format!("Failed to remove hook {}", hook_path.display()))?;
        return Ok(UninstallOutcome::RemovedFile(hook_path));
    }

    fs::write(&hook_path, remaining)
        .with_context(|| format!("Failed to update hook {}", hook_path.display()))?;
    Ok(UninstallOutcome::RemovedBlock(hook_path))
}

/// Inspect the installation status of all supported Git hooks under `root`.
pub fn inspect_hooks(root: &Path) -> Result<Vec<HookStatus>> {
    let hooks_dir = resolve_hooks_dir(root)?;
    let mut statuses = Vec::with_capacity(HookKind::ALL.len());

    for kind in HookKind::ALL {
        let path = hooks_dir.join(kind.file_name());
        let state = inspect_single_hook(&path)?;
        statuses.push(HookStatus { kind, path, state });
    }

    Ok(statuses)
}

fn inspect_single_hook(path: &Path) -> Result<HookState> {
    if !path.exists() {
        return Ok(HookState::NotInstalled);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read hook {}", path.display()))?;
    let executable = is_executable(path)?;

    if let Some((start_idx, end_idx)) = find_marker_bounds(&content) {
        let block = &content[start_idx..end_idx];
        let mode = detect_mode(block);
        return Ok(HookState::Managed { mode, executable });
    }

    Ok(HookState::Unmanaged { executable })
}

fn detect_mode(block: &str) -> Option<HookMode> {
    if block.contains("gate --full") {
        Some(HookMode::GateFull)
    } else if block.contains("cargo-warden gate") {
        Some(HookMode::Gate)
    } else if block.contains("check --staged") {
        Some(HookMode::CheckStaged)
    } else if block.contains("cargo-warden check") {
        Some(HookMode::Check)
    } else {
        None
    }
}

fn find_marker_bounds(content: &str) -> Option<(usize, usize)> {
    let start = content.find(MARKER_START)?;
    let rest = &content[start + MARKER_START.len()..];
    let rel_end = rest.find(MARKER_END)?;
    let end = start + MARKER_START.len() + rel_end + MARKER_END.len();
    Some((start, end))
}

fn split_managed_block(content: &str) -> Option<(&str, &str)> {
    let (start, end) = find_marker_bounds(content)?;
    Some((&content[..start], &content[end..]))
}

fn rebuild_with_block(before: &str, block: &str, after: &str) -> String {
    let trimmed_after = after.strip_prefix('\n').unwrap_or(after);
    let mut out = String::new();
    out.push_str(before);
    out.push_str(block);
    out.push('\n');
    out.push_str(trimmed_after);
    out
}

fn merge_remaining(before: &str, after: &str) -> String {
    let before_trimmed = before.trim_end_matches('\n');
    let after_trimmed = after.trim_start_matches('\n');
    match (before_trimmed.is_empty(), after_trimmed.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("{before_trimmed}\n"),
        (true, false) => format!("{after_trimmed}\n"),
        (false, false) => format!("{before_trimmed}\n\n{after_trimmed}"),
    }
}

fn is_empty_shell_stub(content: &str) -> bool {
    content.lines().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty()
            || trimmed == "#!/usr/bin/env sh"
            || trimmed == "#!/bin/sh"
            || trimmed == "set -eu"
            || trimmed == "set -e"
    })
}

fn write_hook_file(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content)
        .with_context(|| format!("Failed to write hook file {}", path.display()))?;
    set_executable(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .with_context(|| format!("Failed to stat hook file {}", path.display()))?
        .permissions();
    let mode = perms.mode() | 0o755;
    perms.set_mode(mode);
    fs::set_permissions(path, perms)
        .with_context(|| format!("Failed to set executable permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .with_context(|| format!("Failed to stat hook file {}", path.display()))?
        .permissions()
        .mode();
    Ok(mode & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> Result<bool> {
    Ok(true)
}

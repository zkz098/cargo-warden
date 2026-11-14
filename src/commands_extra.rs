use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::commands::{RuntimeContext, finish_single_check, join_paths};
use cargo_warden::checks;
use cargo_warden::config::WardenConfig;
use cargo_warden::report::Severity;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_actions(
    config: WardenConfig,
    base_dir: &Path,
    workflow: Vec<PathBuf>,
    ignore: Vec<String>,
    strict: bool,
    require_sha: bool,
    fix: bool,
    pin_sha: bool,
    offline: bool,
    ctx: &RuntimeContext,
) -> Result<()> {
    let cfg = config.actions.clone().unwrap_or_default();
    let workflows = if workflow.is_empty() {
        &cfg.workflows
    } else {
        &workflow
    };
    let ignored = if ignore.is_empty() {
        &cfg.ignore
    } else {
        &ignore
    };
    let parallel = config.check.as_ref().is_none_or(|c| c.parallel);
    let emit_text = !ctx.format.is_structured();

    if fix || pin_sha {
        let stats = checks::actions::fix_actions(
            base_dir,
            &checks::actions::ActionsOptions {
                workflows,
                ignore: ignored,
                strict: strict || cfg.strict,
                require_sha_pin: require_sha || cfg.require_sha_pin || pin_sha,
                offline,
                changeset: ctx.changeset.as_ref(),
                emit_text,
                parallel,
            },
            fix,
            pin_sha,
        )?;
        if emit_text {
            println!(
                "✨ [actions] updated {} reference(s) across {} file(s)",
                stats.refs_updated, stats.files_updated
            );
        }
        return Ok(());
    }

    let diagnostics = checks::actions::collect_actions_from_config(
        &config,
        base_dir,
        &workflow,
        &ignore,
        strict,
        require_sha,
        offline,
        ctx.changeset.as_ref(),
        emit_text,
    )?;
    finish_single_check(
        diagnostics,
        &config,
        base_dir,
        ctx,
        "Outdated or unpinned GitHub Actions references found",
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_hygiene(
    config: WardenConfig,
    base_dir: &Path,
    paths: Vec<PathBuf>,
    forbid_unsafe: bool,
    forbid_expect: bool,
    forbid_println: bool,
    no_ignore_tests: bool,
    no_gitignore: bool,
    ctx: &RuntimeContext,
) -> Result<()> {
    let mut hyg_cfg = config.hygiene.clone().unwrap_or_default();
    if forbid_unsafe {
        hyg_cfg.forbid_unsafe = true;
    }
    if forbid_expect {
        hyg_cfg.forbid_expect = true;
    }
    if forbid_println {
        hyg_cfg.forbid_println = true;
    }
    if no_ignore_tests {
        hyg_cfg.ignore_tests = false;
    }
    if no_gitignore {
        hyg_cfg.respect_gitignore = false;
    }
    let raw_paths = if paths.is_empty() {
        hyg_cfg.paths.clone()
    } else {
        paths
    };
    let search_paths = join_paths(base_dir, &raw_paths);
    let emit_text = !ctx.format.is_structured();

    let (files_scanned, diagnostics) = checks::hygiene::collect_hygiene(
        &search_paths,
        &checks::hygiene::HygieneOptions {
            base_dir,
            config: &hyg_cfg,
            changeset: ctx.changeset.as_ref(),
            emit_text,
        },
    )?;

    if emit_text
        && !diagnostics
            .iter()
            .any(|d| !d.suppressed && d.severity == Severity::Error)
    {
        println!(
            "[hygiene] OK: scanned {} Rust file(s), all hygiene & safety checks passed",
            files_scanned
        );
    }

    finish_single_check(
        diagnostics,
        &config,
        base_dir,
        ctx,
        "Rust code hygiene or safety violation(s) found",
    )
}

pub(crate) fn handle_manifest(
    config: WardenConfig,
    base_dir: &Path,
    ignore: Vec<String>,
    check_features: bool,
    ctx: &RuntimeContext,
) -> Result<()> {
    let mut manifest_cfg = config.manifest.clone().unwrap_or_default();
    if !ignore.is_empty() {
        manifest_cfg.ignore_crates = ignore;
    }
    if check_features {
        manifest_cfg.check_features = true;
    }
    let emit_text = !ctx.format.is_structured();

    let diagnostics = checks::manifest::collect_manifest(&checks::manifest::ManifestOptions {
        base_dir,
        config: &manifest_cfg,
        changeset: ctx.changeset.as_ref(),
        emit_text,
    })?;

    if emit_text
        && !diagnostics
            .iter()
            .any(|d| !d.suppressed && d.severity == Severity::Error)
    {
        println!("[manifest] OK: workspace manifests satisfy all consistency rules");
    }

    finish_single_check(
        diagnostics,
        &config,
        base_dir,
        ctx,
        "Workspace manifest consistency violation(s) found",
    )
}

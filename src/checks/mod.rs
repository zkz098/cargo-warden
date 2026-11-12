pub mod ci_parity;
pub mod complexity;
pub mod dag;
pub mod file_length;
pub mod i18n_po;
pub mod slint_theme;

use crate::config::WardenConfig;
use anyhow::Result;
use std::path::{Path, PathBuf};

fn join_paths(base_dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|p| base_dir.join(p)).collect()
}

/// Run all static quality checks configured in `WardenConfig`.
pub fn run_static_checks(config: &WardenConfig, base_dir: &Path) -> Result<()> {
    println!("🔍 [warden] running all configured static quality checks...");

    if let Some(fl_cfg) = &config.file_length {
        let search_paths = join_paths(base_dir, &fl_cfg.paths);
        file_length::check_file_length(
            &search_paths,
            fl_cfg.max_lines,
            &fl_cfg.extensions,
            &fl_cfg.ignore,
        )?;
    }

    if let Some(comp_cfg) = &config.complexity {
        let search_paths = join_paths(base_dir, &comp_cfg.paths);
        complexity::check_complexity(
            &search_paths,
            comp_cfg.max,
            comp_cfg.ignore_tests,
            &comp_cfg.ignore,
        )?;
    }

    if let Some(dag_cfg) = &config.dag {
        for r in &dag_cfg.roots {
            dag::check_dag(&base_dir.join(r), &dag_cfg.forbid_imports)?;
        }
    }

    if let Some(ci_cfg) = &config.ci_parity {
        ci_parity::check_ci_parity_with_gate(
            base_dir,
            &ci_cfg.workflow,
            &ci_cfg.test_patterns,
            &ci_cfg.ignore_crates,
            &ci_cfg.fanout_crates,
            config.gate.as_ref(),
        )?;
    }

    if let Some(theme_cfg) = &config.slint_theme
        && base_dir.join(&theme_cfg.ui_dir).exists()
    {
        slint_theme::check_theme(
            &base_dir.join(&theme_cfg.ui_dir),
            &theme_cfg.unchanged_colors,
        )?;
    }

    if let Some(i18n_cfg) = &config.i18n
        && base_dir.join(&i18n_cfg.lang_dir).exists()
    {
        i18n_po::check_i18n(
            &base_dir.join(&i18n_cfg.lang_dir),
            &i18n_cfg.domain,
            &i18n_cfg.locales,
            &base_dir.join(&i18n_cfg.out_file),
        )?;
    }

    println!("✨ [warden] all static quality checks passed successfully!");
    Ok(())
}

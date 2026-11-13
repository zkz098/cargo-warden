//! Shared Rust AST parsing and per-file parallel pipeline for `collect_all_static_checks`.

use super::complexity::{self, ComplexityOptions, ComplexityReport};
use super::dag::{self, DagFileScan, DagOptions, ParsedDagFile};
use super::i18n_po::{self, I18nOptions};
use super::i18n_source::{self, I18nSourceFile, I18nSourceKind, SourceTranslationKey};
use super::parallel;
use super::{CheckTaskOutcome, StaticRunOptions, join_paths};
use crate::config::WardenConfig;
use crate::report::normalize_rel_path;
use crate::suppression::FileSuppressions;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy)]
enum TaskRole {
    Complexity,
    Dag,
    I18n,
}

#[derive(Debug, Clone)]
pub(crate) struct SharedRustTask {
    path: PathBuf,
    want_complexity: bool,
    want_dag: bool,
    want_i18n: bool,
}

#[derive(Default)]
struct TaskRegistry {
    by_key: BTreeMap<PathBuf, usize>,
    tasks: Vec<SharedRustTask>,
}

impl TaskRegistry {
    fn register(&mut self, base_dir: &Path, path: &Path, role: TaskRole) -> usize {
        let key = dedup_key(base_dir, path);
        let idx = *self.by_key.entry(key).or_insert_with(|| {
            let new_idx = self.tasks.len();
            self.tasks.push(SharedRustTask {
                path: path.to_path_buf(),
                want_complexity: false,
                want_dag: false,
                want_i18n: false,
            });
            new_idx
        });
        let task = &mut self.tasks[idx];
        match role {
            TaskRole::Complexity => task.want_complexity = true,
            TaskRole::Dag => task.want_dag = true,
            TaskRole::I18n => task.want_i18n = true,
        }
        idx
    }
}

fn dedup_key(base_dir: &Path, path: &Path) -> PathBuf {
    let rel = normalize_rel_path(base_dir, path);
    let mut cleaned = PathBuf::new();
    for comp in rel.components() {
        if comp != Component::CurDir {
            cleaned.push(comp);
        }
    }
    cleaned
}

pub(crate) struct ComplexityPlan {
    task_indices: Vec<usize>,
    max_complexity: usize,
}

pub(crate) struct DagRootPlan {
    src_root: PathBuf,
    scan: DagFileScan,
    task_indices: Vec<Option<usize>>,
}

pub(crate) struct I18nPlan {
    lang_dir: PathBuf,
    out_file: PathBuf,
    files: Vec<I18nSourceFile>,
    task_indices: Vec<Option<usize>>,
    macro_names: Vec<String>,
}

pub(crate) struct AstCheckPlans {
    pub(crate) complexity: Option<ComplexityPlan>,
    pub(crate) dag_roots: Option<Vec<DagRootPlan>>,
    pub(crate) i18n: Option<I18nPlan>,
    pub(crate) tasks: Vec<SharedRustTask>,
}

pub(crate) struct SharedRustResult {
    suppressions: Option<FileSuppressions>,
    complexity_reports: Option<Vec<ComplexityReport>>,
    dag_imports: Option<ParsedDagFile>,
    i18n_keys: Option<Vec<SourceTranslationKey>>,
    i18n_read_error: Option<String>,
}

pub(crate) fn build_plans(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
) -> Result<AstCheckPlans> {
    let mut registry = TaskRegistry::default();
    let complexity = plan_complexity(config, base_dir, opts, &mut registry);
    let dag_roots = plan_dag(config, base_dir, opts, &mut registry)?;
    let i18n = plan_i18n(config, base_dir, opts, &mut registry);

    Ok(AstCheckPlans {
        complexity,
        dag_roots,
        i18n,
        tasks: registry.tasks,
    })
}

fn plan_complexity(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
    registry: &mut TaskRegistry,
) -> Option<ComplexityPlan> {
    let comp_cfg = config.complexity.as_ref()?;
    let search_paths = join_paths(base_dir, &comp_cfg.paths);
    let comp_opts = ComplexityOptions {
        base_dir,
        max_complexity: comp_cfg.max,
        ignore_tests: comp_cfg.ignore_tests,
        ignore_patterns: &comp_cfg.ignore,
        respect_gitignore: comp_cfg.respect_gitignore,
        changeset: opts.changeset,
        emit_text: false,
    };
    let files = complexity::discover_complexity_files(&search_paths, &comp_opts);
    let task_indices = files
        .iter()
        .map(|p| registry.register(base_dir, p, TaskRole::Complexity))
        .collect();

    Some(ComplexityPlan {
        task_indices,
        max_complexity: comp_cfg.max,
    })
}

fn plan_dag(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
    registry: &mut TaskRegistry,
) -> Result<Option<Vec<DagRootPlan>>> {
    let Some(dag_cfg) = &config.dag else {
        return Ok(None);
    };

    let mut roots = Vec::new();
    for r in &dag_cfg.roots {
        let src_root = base_dir.join(r);
        let dag_opts = DagOptions {
            base_dir,
            forbid_imports: &dag_cfg.forbid_imports,
            layers: &dag_cfg.layers,
            rules: &dag_cfg.rules,
            changeset: opts.changeset,
            emit_text: false,
        };
        if !dag::should_run_dag_for_opts(&src_root, &dag_opts)? {
            continue;
        }
        let scan = dag::scan_dag_files(&src_root);
        let task_indices = scan
            .files
            .iter()
            .map(|file| {
                dag::top_level(&file.module)?;
                Some(registry.register(base_dir, &file.path, TaskRole::Dag))
            })
            .collect();
        roots.push(DagRootPlan {
            src_root,
            scan,
            task_indices,
        });
    }

    Ok(Some(roots))
}

fn plan_i18n(
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
    registry: &mut TaskRegistry,
) -> Option<I18nPlan> {
    let i18n_cfg = config.i18n.as_ref()?;
    let lang_dir = base_dir.join(&i18n_cfg.lang_dir);
    if !lang_dir.exists() {
        return None;
    }
    let out_file = base_dir.join(&i18n_cfg.out_file);
    let i18n_opts = I18nOptions {
        base_dir,
        lang_dir: &lang_dir,
        domain: &i18n_cfg.domain,
        locales: &i18n_cfg.locales,
        out_file: &out_file,
        source_dirs: &i18n_cfg.source_dirs,
        tr_macros: &i18n_cfg.tr_macros,
        strict: i18n_cfg.strict,
        check_placeholders: i18n_cfg.check_placeholders,
        check_unused: i18n_cfg.check_unused,
        respect_gitignore: i18n_cfg.respect_gitignore,
        changeset: opts.changeset,
        emit_text: false,
    };
    if !i18n_po::should_run_i18n(&i18n_opts) {
        return None;
    }

    let resolved_dirs: Vec<PathBuf> = i18n_cfg
        .source_dirs
        .iter()
        .map(|d| base_dir.join(d))
        .filter(|d| d.exists())
        .collect();
    let files = i18n_source::discover_i18n_source_files(
        &resolved_dirs,
        &out_file,
        i18n_cfg.respect_gitignore,
        Some(base_dir),
    );
    let task_indices = files
        .iter()
        .map(|item| match item.kind {
            I18nSourceKind::Rust => Some(registry.register(base_dir, &item.path, TaskRole::I18n)),
            I18nSourceKind::Slint => None,
        })
        .collect();
    let macro_names = i18n_source::resolve_tr_macro_names(&i18n_cfg.tr_macros);

    Some(I18nPlan {
        lang_dir,
        out_file,
        files,
        task_indices,
        macro_names,
    })
}

pub(crate) fn run_shared_ast_tasks(plans: &AstCheckPlans, parallel: bool) -> Vec<SharedRustResult> {
    let macro_refs: Vec<&str> = plans
        .i18n
        .as_ref()
        .map(|p| p.macro_names.iter().map(String::as_str).collect())
        .unwrap_or_default();

    parallel::map_items(&plans.tasks, parallel, |task| {
        execute_single_task(task, &macro_refs)
    })
}

fn execute_single_task(task: &SharedRustTask, macro_names: &[&str]) -> SharedRustResult {
    let content = match fs::read_to_string(&task.path) {
        Ok(c) => c,
        Err(err) => {
            if task.want_complexity {
                tracing::warn!(
                    file = %task.path.display(),
                    error = %format!("Failed to read {}: {err}", task.path.display()),
                    "failed to parse rust file; skipping"
                );
            }
            return SharedRustResult {
                suppressions: None,
                complexity_reports: None,
                dag_imports: None,
                i18n_keys: None,
                i18n_read_error: task
                    .want_i18n
                    .then(|| format!("Failed to read {}: {err}", task.path.display())),
            };
        }
    };

    let syntax = match syn::parse_file(&content) {
        Ok(s) => s,
        Err(err) => {
            if task.want_complexity {
                tracing::warn!(
                    file = %task.path.display(),
                    error = %format!("Failed to parse Rust syntax in {}: {err}", task.path.display()),
                    "failed to parse rust file; skipping"
                );
            }
            if task.want_dag {
                eprintln!(
                    "[dag] warning: skipping {} due to parse error: {err}",
                    task.path.display()
                );
            }
            return SharedRustResult {
                suppressions: None,
                complexity_reports: None,
                dag_imports: None,
                i18n_keys: task.want_i18n.then(Vec::new),
                i18n_read_error: None,
            };
        }
    };

    let suppressions =
        (task.want_complexity || task.want_dag).then(|| FileSuppressions::parse(&content));
    let complexity_reports = task
        .want_complexity
        .then(|| complexity::scan_ast_reports(&task.path, &syntax));
    let dag_imports = task.want_dag.then(|| dag::extract_dag_imports(&syntax));
    let i18n_keys = task
        .want_i18n
        .then(|| i18n_source::extract_rust_keys_from_ast(&task.path, &syntax, macro_names));

    SharedRustResult {
        suppressions,
        complexity_reports,
        dag_imports,
        i18n_keys,
        i18n_read_error: None,
    }
}

pub(crate) fn finish_complexity_outcome(
    plan: Option<&ComplexityPlan>,
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
    tasks: &[SharedRustTask],
    results: &[SharedRustResult],
) -> Option<CheckTaskOutcome> {
    let plan = plan?;
    let comp_cfg = config.complexity.as_ref()?;
    let comp_opts = ComplexityOptions {
        base_dir,
        max_complexity: plan.max_complexity,
        ignore_tests: comp_cfg.ignore_tests,
        ignore_patterns: &comp_cfg.ignore,
        respect_gitignore: comp_cfg.respect_gitignore,
        changeset: opts.changeset,
        emit_text: false,
    };

    let mut report_count = 0usize;
    let mut files_scanned = 0usize;
    let mut diags = Vec::new();

    for &idx in &plan.task_indices {
        let res = &results[idx];
        let (Some(file_reports), Some(suppressions)) = (&res.complexity_reports, &res.suppressions)
        else {
            continue;
        };
        files_scanned += 1;
        report_count += file_reports.len();
        diags.extend(complexity::build_complexity_diagnostics(
            &tasks[idx].path,
            file_reports,
            suppressions,
            &comp_opts,
        ));
    }

    let ok_line = (!diags.iter().any(|d| !d.suppressed)).then(|| {
        format!(
            "[complexity] OK: scanned {} functions across {} files, all within limit ({})",
            report_count, files_scanned, plan.max_complexity
        )
    });
    Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    })
}

pub(crate) fn finish_dag_outcome(
    plans: Option<&[DagRootPlan]>,
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
    results: &[SharedRustResult],
) -> Option<CheckTaskOutcome> {
    let plans = plans?;
    let dag_cfg = config.dag.as_ref()?;
    let dag_opts = DagOptions {
        base_dir,
        forbid_imports: &dag_cfg.forbid_imports,
        layers: &dag_cfg.layers,
        rules: &dag_cfg.rules,
        changeset: opts.changeset,
        emit_text: false,
    };

    let mut dag_diags = Vec::new();
    for root_plan in plans {
        let per_file: Vec<Option<(&ParsedDagFile, &FileSuppressions)>> = root_plan
            .task_indices
            .iter()
            .map(|maybe_idx| {
                let idx = (*maybe_idx)?;
                let res = &results[idx];
                Some((res.dag_imports.as_ref()?, res.suppressions.as_ref()?))
            })
            .collect();
        let (edges, detailed) = dag::assemble_dag_graph(&root_plan.scan, &per_file);
        dag_diags.extend(dag::build_dag_diagnostics(
            &root_plan.src_root,
            &root_plan.scan.nodes,
            &edges,
            &detailed,
            &dag_opts,
        ));
    }

    let ok_line = (!dag_diags.iter().any(|d| !d.suppressed))
        .then(|| "[dag] OK: dependencies form a clean DAG with 0 cycle(s)".to_string());
    Some(CheckTaskOutcome {
        diagnostics: dag_diags,
        ok_line,
    })
}

pub(crate) fn finish_i18n_outcome(
    plan: Option<&I18nPlan>,
    config: &WardenConfig,
    base_dir: &Path,
    opts: &StaticRunOptions<'_>,
    results: &[SharedRustResult],
) -> Result<Option<CheckTaskOutcome>> {
    let Some(plan) = plan else {
        return Ok(None);
    };
    let Some(i18n_cfg) = &config.i18n else {
        return Ok(None);
    };

    let slint_indices: Vec<usize> = plan
        .files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| (f.kind == I18nSourceKind::Slint).then_some(i))
        .collect();
    let slint_results = parallel::map_items(
        &slint_indices,
        opts.parallel,
        |&file_idx| -> Result<Vec<SourceTranslationKey>> {
            let path = &plan.files[file_idx].path;
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            Ok(i18n_source::extract_slint_keys(path, &content))
        },
    );

    let mut slint_iter = slint_results.into_iter();
    let mut source_keys = Vec::new();
    for (i, item) in plan.files.iter().enumerate() {
        match item.kind {
            I18nSourceKind::Slint => {
                if let Some(res) = slint_iter.next() {
                    source_keys.extend(res?);
                }
            }
            I18nSourceKind::Rust => {
                if let Some(task_idx) = plan.task_indices[i] {
                    let res = &results[task_idx];
                    if let Some(err) = &res.i18n_read_error {
                        anyhow::bail!("{err}");
                    }
                    if let Some(keys) = &res.i18n_keys {
                        source_keys.extend(keys.iter().cloned());
                    }
                }
            }
        }
    }

    let i18n_opts = I18nOptions {
        base_dir,
        lang_dir: &plan.lang_dir,
        domain: &i18n_cfg.domain,
        locales: &i18n_cfg.locales,
        out_file: &plan.out_file,
        source_dirs: &i18n_cfg.source_dirs,
        tr_macros: &i18n_cfg.tr_macros,
        strict: i18n_cfg.strict,
        check_placeholders: i18n_cfg.check_placeholders,
        check_unused: i18n_cfg.check_unused,
        respect_gitignore: i18n_cfg.respect_gitignore,
        changeset: opts.changeset,
        emit_text: false,
    };
    let diags = i18n_po::collect_i18n_with_source_keys(&i18n_opts, Some(source_keys))?;
    let ok_line = (!diags.iter().any(|d| !d.suppressed)).then(|| {
        format!(
            "[i18n] OK: {} is in sync with .po catalogs",
            plan.out_file.display()
        )
    });
    Ok(Some(CheckTaskOutcome {
        diagnostics: diags,
        ok_line,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::collect_all_static_checks;
    use crate::config::{ComplexityConfig, DagConfig, FileLengthConfig, I18nConfig};
    use crate::report::OutputFormat;
    use tempfile::tempdir;

    #[test]
    fn shared_ast_pipeline_matches_individual_checks_and_parallel_modes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("lang/en/LC_MESSAGES")).unwrap();

        fs::write(
            root.join("src/a.rs"),
            "pub fn helper() -> &'static str {\n    tr!(\"hello\")\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/b.rs"),
            r#"use crate::a::helper;

pub fn complex(x: i32) -> i32 {
    if x > 0 {
        if x > 1 {
            if x > 2 {
                return x;
            }
        }
    }
    let _ = helper();
    0
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("src/c.rs"),
            "use crate::b::complex;\npub fn c() -> i32 { complex(1) }\n",
        )
        .unwrap();
        fs::write(
            root.join("lang/en/LC_MESSAGES/messages.po"),
            "msgid \"\"\nmsgstr \"\"\n\nmsgid \"hello\"\nmsgstr \"Hello\"\n",
        )
        .unwrap();
        i18n_po::sync_i18n(
            &root.join("lang"),
            "messages",
            &["en".to_string()],
            &root.join("src/i18n_catalog.rs"),
        )
        .unwrap();

        let config = WardenConfig {
            complexity: Some(ComplexityConfig {
                max: 2,
                paths: vec![PathBuf::from(".")],
                ignore_tests: true,
                ignore: vec!["target".to_string()],
                respect_gitignore: false,
            }),
            dag: Some(DagConfig {
                roots: vec![PathBuf::from("src")],
                forbid_imports: vec!["a".to_string()],
                layers: Vec::new(),
                rules: Vec::new(),
            }),
            file_length: Some(FileLengthConfig {
                max_lines: 100,
                extensions: vec!["rs".to_string()],
                paths: vec![PathBuf::from("src")],
                ignore: Vec::new(),
                mode: Default::default(),
                respect_gitignore: false,
            }),
            i18n: Some(I18nConfig {
                lang_dir: PathBuf::from("lang"),
                out_file: PathBuf::from("src/i18n_catalog.rs"),
                domain: "messages".to_string(),
                locales: vec!["en".to_string()],
                source_dirs: vec![PathBuf::from("src")],
                tr_macros: vec!["tr".to_string()],
                strict: true,
                check_placeholders: true,
                check_unused: true,
                respect_gitignore: false,
            }),
            ..Default::default()
        };

        let opts_par = StaticRunOptions {
            format: OutputFormat::Json,
            parallel: true,
            ..Default::default()
        };
        let opts_seq = StaticRunOptions {
            format: OutputFormat::Json,
            parallel: false,
            ..Default::default()
        };

        let plans = build_plans(&config, root, &opts_par).unwrap();
        // src/a.rs, src/b.rs, src/c.rs deduplicated across complexity, dag, and i18n
        // (src/i18n_catalog.rs is excluded from i18n, included in complexity/dag)
        assert_eq!(plans.tasks.len(), 4);
        assert!(plans.tasks.iter().filter(|t| t.want_i18n).count() == 3);
        assert!(plans.tasks.iter().all(|t| t.want_complexity && t.want_dag));

        let par_diags = collect_all_static_checks(&config, root, &opts_par).unwrap();
        let seq_diags = collect_all_static_checks(&config, root, &opts_seq).unwrap();

        let summarize = |ds: &[crate::report::Diagnostic]| {
            ds.iter()
                .map(|d| (d.rule_id.clone(), d.fingerprint.clone(), d.message.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(summarize(&par_diags), summarize(&seq_diags));
        assert!(par_diags.iter().any(|d| d.rule_id == "warden/complexity"));
        assert!(
            par_diags
                .iter()
                .any(|d| d.rule_id == "warden/dag/forbid-import")
        );
    }
}

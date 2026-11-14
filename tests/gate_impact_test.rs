use cargo_warden::config::{GateConfig, TestGroupConfig};
use cargo_warden::gate::impact::{
    AffectedScope, WorkspaceGraph, WorkspaceMember, compute_affected_from_graph,
};
use cargo_warden::gate::steps::build_filtered_all_steps_with_scope;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

fn sample_graph() -> WorkspaceGraph {
    let members = vec![
        WorkspaceMember {
            name: "root-app".to_string(),
            rel_dir: ".".to_string(),
        },
        WorkspaceMember {
            name: "ws-types".to_string(),
            rel_dir: "crates/types".to_string(),
        },
        WorkspaceMember {
            name: "ws-core".to_string(),
            rel_dir: "crates/core".to_string(),
        },
        WorkspaceMember {
            name: "ws-dev-harness".to_string(),
            rel_dir: "crates/dev-harness".to_string(),
        },
        WorkspaceMember {
            name: "ws-unrelated".to_string(),
            rel_dir: "crates/unrelated".to_string(),
        },
    ];

    let mut rev_normal: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    rev_normal
        .entry("ws-types".to_string())
        .or_default()
        .insert("ws-core".to_string());
    rev_normal
        .entry("ws-core".to_string())
        .or_default()
        .insert("root-app".to_string());
    // Downstream of dev-harness should NOT be pulled in when dev-harness is only affected via dev edge
    rev_normal
        .entry("ws-dev-harness".to_string())
        .or_default()
        .insert("ws-unrelated".to_string());

    let mut rev_dev: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    rev_dev
        .entry("ws-types".to_string())
        .or_default()
        .insert("ws-dev-harness".to_string());

    WorkspaceGraph {
        members,
        rev_normal,
        rev_dev,
    }
}

#[test]
fn test_reverse_closure_and_dev_one_hop() {
    let graph = sample_graph();
    let changed = BTreeSet::from([PathBuf::from("crates/types/src/lib.rs")]);
    let scope = compute_affected_from_graph(&graph, &changed);

    let expected = BTreeSet::from([
        "ws-types".to_string(),
        "ws-core".to_string(),
        "root-app".to_string(),
        "ws-dev-harness".to_string(),
    ]);
    assert_eq!(
        scope,
        AffectedScope::Packages {
            packages: expected,
            total_members: 5
        }
    );
}

#[test]
fn test_longest_prefix_and_global_trigger() {
    let graph = sample_graph();

    // File inside crates/unrelated belongs to ws-unrelated, not root-app (".")
    let unrelated = BTreeSet::from([PathBuf::from("crates/unrelated/src/lib.rs")]);
    assert_eq!(
        compute_affected_from_graph(&graph, &unrelated),
        AffectedScope::Packages {
            packages: BTreeSet::from(["ws-unrelated".to_string()]),
            total_members: 5
        }
    );

    // Global trigger forces AffectedScope::All
    let lockfile = BTreeSet::from([PathBuf::from("Cargo.lock")]);
    assert_eq!(
        compute_affected_from_graph(&graph, &lockfile),
        AffectedScope::All
    );
}

#[test]
fn test_non_package_docs_change_skips_cargo_steps() {
    let graph = WorkspaceGraph {
        members: vec![
            WorkspaceMember {
                name: "ws-types".to_string(),
                rel_dir: "crates/types".to_string(),
            },
            WorkspaceMember {
                name: "ws-core".to_string(),
                rel_dir: "crates/core".to_string(),
            },
        ],
        ..Default::default()
    };

    let docs = BTreeSet::from([PathBuf::from("README.md")]);
    let scope = compute_affected_from_graph(&graph, &docs);
    assert_eq!(
        scope,
        AffectedScope::None {
            has_rust_changes: false
        }
    );

    let steps =
        build_filtered_all_steps_with_scope(&GateConfig::default(), false, false, &[], &[], &scope);
    assert!(steps.is_empty(), "expected no cargo steps, got {steps:#?}");
}

#[test]
fn test_steps_scoped_with_test_groups() {
    let cfg = GateConfig {
        test_groups: vec![
            TestGroupConfig {
                name: "core".to_string(),
                packages: vec!["ws-types".to_string(), "ws-core".to_string()],
                features: vec![],
            },
            TestGroupConfig {
                name: "other".to_string(),
                packages: vec!["ws-unrelated".to_string()],
                features: vec![],
            },
        ],
        ..Default::default()
    };

    let scope = AffectedScope::Packages {
        packages: BTreeSet::from(["ws-core".to_string()]),
        total_members: 3,
    };

    let steps = build_filtered_all_steps_with_scope(&cfg, false, false, &[], &[], &scope);
    let rendered: Vec<(String, String)> = steps
        .iter()
        .map(|s| (s.name.clone(), s.rendered()))
        .collect();

    assert_eq!(rendered.len(), 4, "{rendered:#?}");
    assert_eq!(rendered[0].0, "fmt");
    assert!(rendered[1].1.contains("-p ws-core"), "{}", rendered[1].1);
    assert!(!rendered[1].1.contains("--workspace"), "{}", rendered[1].1);
    assert_eq!(rendered[2].0, "test");
    assert!(rendered[2].1.contains("-p ws-core"), "{}", rendered[2].1);
    assert!(!rendered[2].1.contains("ws-types"), "{}", rendered[2].1);
    assert_eq!(rendered[3].0, "doc");
    assert!(rendered[3].1.contains("-p ws-core"), "{}", rendered[3].1);
}

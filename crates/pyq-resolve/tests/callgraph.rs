//! Integration test for the transitive call graph (#10) over the in-repo
//! `examples/sample` project. Exercises the FQN-keyed forward/reverse closure
//! and the durable-id contract directly against the library API.

use pyq_index::extract;
use pyq_resolve::{CallGraph, Direction};
use std::collections::HashSet;
use std::path::PathBuf;

fn sample_root() -> String {
    format!("{}/../../examples/sample", env!("CARGO_MANIFEST_DIR"))
}

/// Build a [`CallGraph`] over `examples/sample` with no result filtering (the
/// CLI walk applies the file discipline; here we resolve directly).
fn graph() -> CallGraph {
    let root = sample_root();
    let mut files = Vec::new();
    let mut walk = vec![PathBuf::from(&root)];
    while let Some(dir) = walk.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
                let rel = path.strip_prefix(&root).unwrap().to_string_lossy().into_owned();
                let src = std::fs::read_to_string(&path).unwrap();
                files.push(extract(&rel, &src));
            }
        }
    }
    CallGraph::new(&root, files, HashSet::new()).expect("build call graph")
}

fn fqns(nodes: &[pyq_resolve::GraphNode]) -> HashSet<String> {
    nodes.iter().map(|n| n.fqn.clone()).collect()
}

#[test]
fn forward_closure_resolves_to_stable_fqns_across_files() {
    let g = graph();
    let closure = g.closure("main", Direction::Forward, None);
    assert_eq!(closure.roots, vec!["app.main".to_string()]);
    let reached = fqns(&closure.nodes);
    // `main` calls `make_user` and `User`, both in pkg/models.py — cross-file,
    // and addressed by durable FQNs rather than line numbers.
    assert!(reached.contains("pkg.models.make_user"), "{reached:?}");
    assert!(reached.contains("pkg.models.User"), "{reached:?}");
}

#[test]
fn reverse_closure_finds_transitive_callers() {
    let g = graph();
    let closure = g.closure("User", Direction::Reverse, None);
    let reached = fqns(&closure.nodes);
    // `make_user` constructs `User` directly; `main` reaches it via make_user.
    assert!(reached.contains("pkg.models.make_user"), "{reached:?}");
    assert!(reached.contains("app.main"), "transitive caller: {reached:?}");
}

#[test]
fn depth_one_keeps_only_direct_neighbours() {
    let g = graph();
    // `main` and `make_user` both construct `User` directly (depth 1); the
    // module `app` reaches it only transitively via `main` (depth 2).
    let direct = g.closure("User", Direction::Reverse, Some(1));
    assert!(direct.nodes.iter().all(|n| n.depth == 1));
    let reached = fqns(&direct.nodes);
    assert!(reached.contains("pkg.models.make_user"));
    assert!(reached.contains("app.main"));
    assert!(!reached.contains("app"), "depth-2 module caller dropped: {reached:?}");

    // Unbounded, the module's transitive reach is included.
    let full = fqns(&g.closure("User", Direction::Reverse, None).nodes);
    assert!(full.contains("app"), "transitive module caller: {full:?}");
}

#[test]
fn an_uncallable_name_yields_no_roots() {
    let g = graph();
    let closure = g.closure("name", Direction::Forward, None);
    // `name` is a parameter/attribute, never a function or class def.
    assert!(closure.roots.is_empty(), "roots: {:?}", closure.roots);
    assert!(closure.nodes.is_empty());
}

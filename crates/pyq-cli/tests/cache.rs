//! The analysis cache (#38.2): a warm run must be byte-for-byte identical to a
//! cold one, and an edit/add/remove must invalidate exactly the changed file.
//!
//! Drives the pure-syntactic `inputs` verb (no ty, no Python) so these tests
//! exercise the parse layer directly, and points `PYQ_CACHE_DIR` at a tempdir so
//! they never touch the developer's real `~/.pyq`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Absolute path to `examples/sample` at the workspace root.
fn sample_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/sample")
        .canonicalize()
        .expect("examples/sample should exist")
}

/// Run `pyq <args> --root <root>` with the cache redirected to `cache_dir`,
/// returning raw stdout (so a cold/warm comparison is byte-exact).
fn run(root: &Path, cache_dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(args)
        .arg("--root")
        .arg(root)
        .env("PYQ_CACHE_DIR", cache_dir)
        .output()
        .expect("pyq should run");
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

/// Run `pyq inputs --root <proj> --json` with the cache redirected to
/// `cache_dir`, returning the parsed envelope.
fn inputs(proj: &Path, cache_dir: &Path) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["inputs", "--json", "--root"])
        .arg(proj)
        .env("PYQ_CACHE_DIR", cache_dir)
        .output()
        .expect("pyq should run");
    assert!(out.status.success(), "pyq inputs should succeed");
    let stdout = String::from_utf8(out.stdout).expect("utf-8");
    serde_json::from_str(stdout.trim()).expect("envelope is JSON")
}

/// The sorted `label`s of an envelope's results.
fn labels(env: &Value) -> Vec<String> {
    let mut v: Vec<String> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn warm_run_matches_cold_and_persists_a_cache() {
    let proj = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    std::fs::write(proj.path().join("app.py"), "import os\nos.getenv(\"ALPHA\")\n").unwrap();

    // Cold: builds the cache.
    let cold = inputs(proj.path(), cache.path());
    assert!(labels(&cold).iter().any(|l| l == "env ALPHA"));

    // A `parse.bin` should now exist under a per-root namespace dir.
    let has_parse_bin = std::fs::read_dir(cache.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().join("parse.bin").exists());
    assert!(has_parse_bin, "cold run should persist parse.bin");

    // Warm: identical output, served from the cache.
    let warm = inputs(proj.path(), cache.path());
    assert_eq!(cold, warm, "warm run must match cold run exactly");
}

#[test]
fn an_edit_invalidates_only_that_file_even_at_equal_size() {
    let proj = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let app = proj.path().join("app.py");
    std::fs::write(&app, "import os\nos.getenv(\"ALPHA\")\n").unwrap();

    assert!(labels(&inputs(proj.path(), cache.path()))
        .iter()
        .any(|l| l == "env ALPHA"));

    // Rewrite to a same-length key: size is unchanged, so only a content hash
    // (not size+mtime) can catch this — the branch that guards against a coarse
    // stat falsely reusing a stale parse.
    std::fs::write(&app, "import os\nos.getenv(\"GAMMA\")\n").unwrap();
    let after = labels(&inputs(proj.path(), cache.path()));
    assert!(after.iter().any(|l| l == "env GAMMA"), "edit must be seen");
    assert!(
        !after.iter().any(|l| l == "env ALPHA"),
        "stale value must be gone"
    );
}

#[test]
fn added_and_removed_files_update_the_cache() {
    let proj = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let a = proj.path().join("a.py");
    let b = proj.path().join("b.py");
    std::fs::write(&a, "import os\nos.getenv(\"ALPHA\")\n").unwrap();

    inputs(proj.path(), cache.path()); // warm

    // Add a file → its inputs appear without a forced rebuild.
    std::fs::write(&b, "import os\nos.getenv(\"BETA\")\n").unwrap();
    let two = labels(&inputs(proj.path(), cache.path()));
    assert!(two.iter().any(|l| l == "env ALPHA"));
    assert!(two.iter().any(|l| l == "env BETA"));

    // Remove a file → its inputs disappear.
    std::fs::remove_file(&a).unwrap();
    let one = labels(&inputs(proj.path(), cache.path()));
    assert!(!one.iter().any(|l| l == "env ALPHA"), "removed file must drop out");
    assert!(one.iter().any(|l| l == "env BETA"));
}

/// The graph layer (#38.3): the cold run builds the live call graph and records
/// its full ty-query surface; the warm run replays that recording with no ty.
/// Every graph-backed verb must produce byte-identical output across the two —
/// if the recording misses a query the replay returns empty and this diverges.
#[test]
fn graph_verbs_replay_identically_cold_and_warm() {
    let root = sample_root();
    // One fresh cache dir for the whole sequence: the first invocation of each
    // verb is cold (records), the rest are warm (replay the same graph.bin).
    let cache = TempDir::new().unwrap();

    let verbs: &[&[&str]] = &[
        &["graph", "make_user"],
        &["graph", "make_user", "--reverse"],
        &["graph", "User", "--reverse"],
        &["effects", "make_user"],
        &["tests", "make_user"],
        &["describe", "make_user"],
        &["hierarchy", "User"],
        &["deadcode"],
        &["canonical"],
    ];

    for verb in verbs {
        let mut args = verb.to_vec();
        args.push("--json");
        // First call may be cold (builds the graph cache) or warm (a prior verb
        // already built it for this tree); either way a second call is warm.
        let first = run(&root, cache.path(), &args);
        let second = run(&root, cache.path(), &args);
        assert_eq!(
            first, second,
            "warm replay must match for `{}`",
            verb.join(" ")
        );
        // And the warm result must match a from-scratch ty run (no cache at all).
        let nocache = {
            let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
                .args(&args)
                .arg("--root")
                .arg(&root)
                .env("PYQ_NO_CACHE", "1")
                .output()
                .expect("pyq should run");
            String::from_utf8(out.stdout).expect("utf-8")
        };
        assert_eq!(
            second, nocache,
            "replayed graph must match live ty for `{}`",
            verb.join(" ")
        );
    }

    // The graph recording was persisted.
    let has_graph_bin = std::fs::read_dir(cache.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().join("graph.bin").exists());
    assert!(has_graph_bin, "a graph.bin recording should be persisted");
}

/// Incremental graph invalidation (#38.5): after a warm graph cache, an edit /
/// add / delete must repair the recording so the result is byte-identical to a
/// from-scratch (`PYQ_NO_CACHE`) build — only cheaper. Uses pure-graph verbs
/// (`graph`/`deadcode`/`canonical`) so no test suite runs.
#[test]
fn graph_cache_repairs_incrementally_and_matches_from_scratch() {
    let proj = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let p = proj.path();
    std::fs::create_dir_all(p.join("pkg")).unwrap();
    std::fs::write(p.join("pkg/__init__.py"), "").unwrap();
    // a and b independent; c imports a.
    std::fs::write(p.join("pkg/a.py"), "def a1():\n    return 1\n").unwrap();
    std::fs::write(p.join("pkg/b.py"), "def b1():\n    return 2\n").unwrap();
    std::fs::write(p.join("pkg/c.py"), "from pkg.a import a1\ndef c1():\n    return a1()\n").unwrap();

    // Compare every graph verb between the (warm, incrementally-repaired) cache
    // and a from-scratch build, asserting byte-equality.
    let verbs: &[&[&str]] = &[
        &["graph", "c1", "--json"],
        &["graph", "a1", "--reverse", "--json"],
        &["graph", "b1", "--reverse", "--json"],
        &["deadcode", "--json"],
        &["canonical", "--json"],
    ];
    let assert_matches = |cache_dir: &Path, label: &str| {
        for v in verbs {
            let incremental = run(p, cache_dir, v);
            let scratch = {
                let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
                    .args(*v)
                    .arg("--root")
                    .arg(p)
                    .env("PYQ_NO_CACHE", "1")
                    .output()
                    .unwrap();
                String::from_utf8(out.stdout).unwrap()
            };
            assert_eq!(incremental, scratch, "{label}: `{}`", v.join(" "));
        }
    };

    // Warm the cache, then exercise each kind of change.
    run(p, cache.path(), &["graph", "c1", "--json"]);

    std::fs::write(p.join("pkg/b.py"), "def b2():\n    return 3\ndef b1():\n    return b2()\n").unwrap();
    assert_matches(cache.path(), "edit");

    std::fs::write(p.join("pkg/c.py"), "from pkg.b import b1\ndef c1():\n    return b1()\n").unwrap();
    assert_matches(cache.path(), "cross-file edge change");

    std::fs::write(p.join("pkg/d.py"), "from pkg.c import c1\ndef d1():\n    return c1()\n").unwrap();
    assert_matches(cache.path(), "add file");

    std::fs::remove_file(p.join("pkg/d.py")).unwrap();
    assert_matches(cache.path(), "delete file");
}

/// `pyq index` prewarms both layers; `pyq index clean` wipes this repo's cache.
#[test]
fn index_verb_prewarms_and_clean_wipes() {
    let root = sample_root();
    let cache = TempDir::new().unwrap();

    // `index` writes both the parse and graph layers under one per-root dir.
    let out = run(&root, cache.path(), &["index"]);
    assert!(out.contains("indexed"), "index reports what it did: {out}");
    let repo_dir = std::fs::read_dir(cache.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.join("parse.bin").exists())
        .expect("index should create a per-root cache dir");
    assert!(repo_dir.join("graph.bin").exists(), "index records the graph");

    // After indexing, a graph verb matches a from-scratch (no-cache) run.
    let warm = run(&root, cache.path(), &["deadcode", "--json"]);
    let nocache = {
        let o = Command::new(env!("CARGO_BIN_EXE_pyq"))
            .args(["deadcode", "--json", "--root"])
            .arg(&root)
            .env("PYQ_NO_CACHE", "1")
            .output()
            .unwrap();
        String::from_utf8(o.stdout).unwrap()
    };
    assert_eq!(warm, nocache, "indexed run must match live ty");

    // `index clean` removes the repo's cache dir.
    let cleaned = run(&root, cache.path(), &["index", "clean"]);
    assert!(cleaned.contains("removed"), "clean reports removal: {cleaned}");
    assert!(!repo_dir.exists(), "clean wipes the per-root dir");

    // Cleaning again is a no-op, reported plainly.
    let again = run(&root, cache.path(), &["index", "clean"]);
    assert!(again.contains("no cached index"), "second clean is a no-op: {again}");
}

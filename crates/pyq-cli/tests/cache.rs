//! The analysis cache (#38.2): a warm run must be byte-for-byte identical to a
//! cold one, and an edit/add/remove must invalidate exactly the changed file.
//!
//! Drives the pure-syntactic `inputs` verb (no ty, no Python) so these tests
//! exercise the parse layer directly, and points `PYQ_CACHE_DIR` at a tempdir so
//! they never touch the developer's real `~/.pyq`.

use serde_json::Value;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

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

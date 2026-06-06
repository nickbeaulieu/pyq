//! End-to-end tests: run the built `pyq` binary against `examples/sample` and
//! assert on the JSON envelope. Covers the dispatch wiring, both engines (ty
//! and `--syntactic`), and the shared output shape.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

/// Absolute path to `examples/sample` at the workspace root.
fn sample_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/sample")
        .canonicalize()
        .expect("examples/sample should exist")
}

/// Absolute path to a named fixture under `tests/fixtures/`.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .canonicalize()
        .unwrap_or_else(|_| panic!("fixture {name} should exist"))
}

/// Run `pyq <args> --root examples/sample --json` and return (parsed envelope,
/// success flag).
fn run_json(args: &[&str]) -> (Value, bool) {
    run_json_in(&sample_root(), args)
}

/// Like [`run_json`] but against an arbitrary project root.
fn run_json_in(root: &std::path::Path, args: &[&str]) -> (Value, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(args)
        .arg("--root")
        .arg(root)
        .arg("--json")
        .output()
        .expect("pyq should run");
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let env: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("envelope should be JSON: {e}\n--- stdout ---\n{stdout}"));
    (env, out.status.success())
}

/// The `label`s of every result in an envelope.
fn labels(env: &Value) -> Vec<String> {
    env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect()
}

/// The `loc`s of every result in an envelope.
fn locs(env: &Value) -> Vec<String> {
    env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["loc"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn envelope_has_the_shared_shape() {
    let (env, ok) = run_json(&["inputs"]);
    assert!(ok);
    assert_eq!(env["tool"], "pyq");
    assert_eq!(env["query"]["kind"], "inputs");
    assert_eq!(
        env["count"].as_u64().unwrap(),
        env["results"].as_array().unwrap().len() as u64,
        "count must match results length"
    );
}

#[test]
fn inputs_surfaces_env_files_args_and_settings() {
    let (env, ok) = run_json(&["inputs"]);
    assert!(ok);
    let labels = labels(&env);

    // env reads, including the bucketed computed key
    assert!(labels.contains(&"env DATABASE_URL".to_string()));
    assert!(labels.contains(&"env <dynamic>".to_string()));
    // literal file opened
    assert!(labels.contains(&"file settings.ini".to_string()));
    // argparse + click args
    assert!(labels.contains(&"arg --verbose".to_string()));
    assert!(labels.contains(&"arg --count".to_string()));
    // pydantic BaseSettings fields (annotated only)
    assert!(labels.contains(&"setting db_url".to_string()));
    assert!(
        !labels.contains(&"setting debug".to_string()),
        "unannotated class attr must not be a setting"
    );
}

#[test]
fn defs_syntactic_finds_function_and_import_binding() {
    let (env, ok) = run_json(&["defs", "make_user", "--syntactic"]);
    assert!(ok);
    assert_eq!(env["query"]["kind"], "defs");
    let locs = locs(&env);
    let labels = labels(&env);
    // defined as a function in pkg/models.py
    assert!(locs.iter().any(|l| l.starts_with("pkg/models.py")));
    assert!(labels.iter().any(|l| l == "function"));
    // bound by `from pkg.models import make_user` in app.py
    assert!(locs.iter().any(|l| l.starts_with("app.py")));
    assert!(labels.iter().any(|l| l == "import"));
}

#[test]
fn refs_via_ty_span_multiple_files() {
    let (env, ok) = run_json(&["refs", "User"]);
    assert!(ok);
    assert_eq!(env["query"]["engine"], "ty");
    let files: std::collections::HashSet<_> = locs(&env)
        .iter()
        .map(|l| l.split(':').next().unwrap().to_string())
        .collect();
    // `User` is defined in pkg/models.py and used in app.py — cross-file.
    assert!(files.contains("pkg/models.py"));
    assert!(files.contains("app.py"));
}

#[test]
fn callers_via_ty_finds_the_call_site() {
    let (env, ok) = run_json(&["callers", "make_user"]);
    assert!(ok);
    assert_eq!(env["query"]["kind"], "callers");
    // `make_user("ada")` is called inside app.py's `main`.
    assert!(locs(&env).iter().any(|l| l.starts_with("app.py")));
}

#[test]
fn unknown_symbol_is_zero_results_not_an_error() {
    let (env, ok) = run_json(&["defs", "NoSuchSymbolAnywhere", "--syntactic"]);
    assert!(ok, "an unknown symbol should exit 0");
    assert_eq!(env["count"].as_u64().unwrap(), 0);
}

#[test]
fn imports_lists_edges_and_marks_external() {
    let (env, ok) = run_json(&["imports"]);
    assert!(ok);
    assert_eq!(env["query"]["mode"], "all");
    let labels = labels(&env);
    // internal edge has no marker; stdlib/third-party is tagged (ext).
    assert!(labels.iter().any(|l| l == "app → pkg.models"));
    assert!(labels.iter().any(|l| l == "config → os (ext)"));
}

#[test]
fn imports_forward_and_reverse_are_inverse_views() {
    let (fwd, ok) = run_json(&["imports", "app"]);
    assert!(ok);
    assert_eq!(fwd["query"]["mode"], "forward");
    assert!(labels(&fwd).iter().any(|l| l == "imports pkg.models"));

    // Reverse accepts a file path too, and points back at the importer.
    let (rev, ok) = run_json(&["imports", "pkg/models.py", "--reverse"]);
    assert!(ok);
    assert_eq!(rev["query"]["mode"], "reverse");
    assert!(labels(&rev).iter().any(|l| l == "imported by app"));
    assert!(locs(&rev).iter().any(|l| l.starts_with("app.py")));
}

#[test]
fn cycles_detects_the_mutual_import() {
    let (env, ok) = run_json_in(&fixture("cycle"), &["imports", "--cycles"]);
    assert!(ok);
    assert_eq!(env["count"].as_u64().unwrap(), 1);
    let label = env["results"][0]["label"].as_str().unwrap();
    assert!(label.starts_with("cycle:"));
    assert!(label.contains("pkg.a"));
    assert!(label.contains("pkg.b"));
}

#[test]
fn sample_has_no_cycles() {
    let (env, ok) = run_json(&["imports", "--cycles"]);
    assert!(ok);
    assert_eq!(env["count"].as_u64().unwrap(), 0);
}

// Regression: ty must inherit the CLI walk's --root scoping. `User` is used in
// both app.py and pkg/models.py; scoping the root to the pkg subtree must drop
// the app.py reference and report paths anchored to that root.
#[test]
fn ty_refs_honor_root_scoping() {
    let pkg = sample_root().join("pkg");
    let (env, ok) = run_json_in(&pkg, &["refs", "User"]);
    assert!(ok);
    let files: std::collections::HashSet<_> = locs(&env)
        .iter()
        .map(|l| l.split(':').next().unwrap().to_string())
        .collect();
    assert!(
        files.contains("models.py"),
        "in-scope file should appear, anchored to root: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("app.py")),
        "out-of-scope app.py must be filtered: {files:?}"
    );
}

// Regression: a file with a trailing syntax error must still answer for the
// statements that parsed before the error ("half-edited file still answers").
#[test]
fn broken_file_still_answers_for_pre_error_statements() {
    let root = fixture("broken");
    let (inputs, ok) = run_json_in(&root, &["inputs"]);
    assert!(ok);
    assert!(
        labels(&inputs).iter().any(|l| l == "env EARLY_KEY"),
        "env read before the error should survive: {:?}",
        labels(&inputs)
    );

    let (defs, ok) = run_json_in(&root, &["defs", "alpha", "--syntactic"]);
    assert!(ok);
    assert!(labels(&defs).iter().any(|l| l == "function"));
}

#[test]
fn human_view_is_a_summary_line_then_results() {
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["inputs", "--root"])
        .arg(sample_root())
        .output()
        .expect("pyq should run");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let first = stdout.lines().next().unwrap_or_default();
    assert!(first.ends_with("inputs"), "summary line was: {first:?}");
    assert!(stdout.contains("settings.ini"));
}

//! End-to-end tests for the `describe` verb (#15) over the built `pyq` binary.
//!
//! `describe` packs a symbol's static facets (signature, decorators, docstring,
//! def line-span) together with its depth-1 call neighbourhood (immediate
//! callers + callees) and the collected tests that reach it — so these assert on
//! the rows by `role`, not position.

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Write a small project tree under a fresh temp dir and return it.
fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

/// Run `pyq describe <symbol> --root <root> --json` and parse the envelope.
fn describe(root: &Path, symbol: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(root)
        .args(["describe", symbol])
        .output()
        .expect("pyq should run");
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("envelope should be JSON: {e}\n{}", String::from_utf8_lossy(&out.stdout)))
}

/// The single result row with the given `role` (panics if not exactly one).
fn row_with_role<'a>(env: &'a Value, role: &str) -> &'a Value {
    let mut it = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["role"] == role);
    let row = it.next().unwrap_or_else(|| panic!("no `{role}` row in {env}"));
    assert!(it.next().is_none(), "more than one `{role}` row");
    row
}

/// Every `fqn` among rows of the given `role`.
fn fqns_with_role(env: &Value, role: &str) -> Vec<String> {
    env["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["role"] == role)
        .map(|r| r["fqn"].as_str().unwrap().to_string())
        .collect()
}

const PKG_INIT: (&str, &str) = ("pkg/__init__.py", "");

#[test]
fn describe_packs_signature_decorators_doc_span_and_neighbourhood() {
    let dir = project(&[
        PKG_INIT,
        (
            "pkg/calc.py",
            "import functools\n\n\n@functools.lru_cache\ndef add(a: int, b: int = 1) -> int:\n    \"\"\"Add two numbers.\"\"\"\n    return a + b\n\n\ndef total(xs):\n    s = 0\n    for x in xs:\n        s = add(s, x)\n    return s\n",
        ),
        (
            "tests/test_calc.py",
            "from pkg.calc import total\n\n\ndef test_total():\n    assert total([1, 2, 3]) == 6\n",
        ),
    ]);
    let root = dir.path();

    // --- the definition facet of `add` ---
    let env = describe(root, "add");
    assert_eq!(env["tool"], "pyq");
    assert_eq!(env["query"]["kind"], "describe");
    assert_eq!(env["query"]["roots"][0], "pkg.calc.add");

    let def = row_with_role(&env, "definition");
    assert_eq!(def["fqn"], "pkg.calc.add");
    assert_eq!(def["node_kind"], "def");
    assert_eq!(def["signature"], "(a: int, b: int = 1) -> int");
    assert_eq!(def["decorators"][0], "functools.lru_cache");
    assert_eq!(def["doc"], "Add two numbers.");
    // The span is [def line, last body line] — here the 3-line def at line 5.
    assert_eq!(def["lines"][0], 5);
    assert_eq!(def["lines"][1], 7);
    assert_eq!(def["loc"], "pkg/calc.py:5:5");

    // `add` is called once (by `total`) and calls nothing first-party.
    assert_eq!(fqns_with_role(&env, "caller"), vec!["pkg.calc.total"]);
    assert!(fqns_with_role(&env, "callee").is_empty());
    // A test reaches it transitively (test_total → total → add): depth 2.
    let test = row_with_role(&env, "test");
    assert_eq!(test["fqn"], "tests.test_calc.test_total");
    assert_eq!(test["depth"], 2);

    // --- `total` sees the inverse: it calls `add`, a test calls it directly ---
    let env = describe(root, "total");
    assert_eq!(fqns_with_role(&env, "callee"), vec!["pkg.calc.add"]);
    assert_eq!(fqns_with_role(&env, "caller"), vec!["tests.test_calc.test_total"]);
    let test = row_with_role(&env, "test");
    assert_eq!(test["depth"], 1, "test calls total directly");
}

#[test]
fn describe_renders_a_class_with_bases_and_no_function_signature() {
    let dir = project(&[
        PKG_INIT,
        (
            "pkg/models.py",
            "class Base:\n    pass\n\n\nclass User(Base):\n    \"\"\"A user.\"\"\"\n    def __init__(self, name):\n        self.name = name\n",
        ),
    ]);
    let env = describe(dir.path(), "User");
    let def = row_with_role(&env, "definition");
    assert_eq!(def["node_kind"], "class");
    // A class's "signature" is its base list; its docstring still shows.
    assert_eq!(def["signature"], "(Base)");
    assert_eq!(def["doc"], "A user.");
    assert!(def["label"].as_str().unwrap().contains("class User(Base)"));
}

#[test]
fn unknown_symbol_is_zero_results_with_a_not_found_warning() {
    let dir = project(&[PKG_INIT, ("pkg/m.py", "def real():\n    return 1\n")]);
    let env = describe(dir.path(), "nope");
    assert_eq!(env["count"], 0);
    assert_eq!(env["query"]["roots"].as_array().unwrap().len(), 0);
    let warnings = env["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("no function or class named `nope`")),
        "not-found warning expected: {warnings:?}"
    );
}

#[test]
fn ambiguous_name_emits_a_definition_per_root_and_warns() {
    let dir = project(&[
        PKG_INIT,
        ("pkg/a.py", "def proc():\n    return 1\n"),
        ("pkg/b.py", "def proc():\n    return 2\n"),
    ]);
    let env = describe(dir.path(), "proc");
    // Both defs resolve; one definition row apiece.
    let defs: Vec<String> = fqns_with_role(&env, "definition");
    assert!(defs.contains(&"pkg.a.proc".to_string()), "{defs:?}");
    assert!(defs.contains(&"pkg.b.proc".to_string()), "{defs:?}");
    let warnings = env["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("ambiguous")),
        "ambiguity warning expected: {warnings:?}"
    );
}

#[test]
fn empty_symbol_is_a_usage_error() {
    let dir = project(&[PKG_INIT, ("pkg/m.py", "def f():\n    return 1\n")]);
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(dir.path())
        .args(["describe", "   "])
        .output()
        .expect("pyq should run");
    assert!(!out.status.success(), "blank symbol must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("symbol must not be empty"), "stderr: {stderr}");
}

//! End-to-end tests: run the built `pyq` binary against `examples/sample` and
//! assert on the JSON envelope. Covers the dispatch wiring, the unified engine
//! (ty ∪ syntactic) and its `--syntactic` debug filter, and the shared shape.

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

// The query block is uniform across verbs: every one carries kind, target
// (null where there's no single target), and engine — so an agent can branch
// on query.engine / query.target without per-verb special-casing.
#[test]
fn query_block_is_uniform_across_verbs() {
    for (args, engine) in [
        (vec!["refs", "User"], "unified"),
        (vec!["defs", "User", "--syntactic"], "syntactic"),
        (vec!["inputs"], "syntactic"),
        (vec!["imports"], "syntactic"),
    ] {
        let (env, ok) = run_json(&args);
        assert!(ok, "{args:?}");
        let q = &env["query"];
        assert!(q.get("kind").is_some(), "kind missing: {args:?}");
        assert!(q.get("target").is_some(), "target key missing (null ok): {args:?}");
        assert_eq!(q["engine"], engine, "engine for {args:?}");
    }
}

// Determinism: the resolved (canonical, absolute) root is echoed in the query,
// so an agent gets the same anchored answer regardless of the working dir.
#[test]
fn query_echoes_the_resolved_absolute_root() {
    let (env, ok) = run_json(&["defs", "User"]);
    assert!(ok);
    let root = env["query"]["root"].as_str().expect("root in query");
    assert!(root.starts_with('/'), "root should be absolute: {root}");
    assert!(root.ends_with("examples/sample"), "root: {root}");
}

// The --syntactic debug path can't see attribute-access calls, so it flags that
// a count may be incomplete rather than letting a bare 0 read as ground truth.
#[test]
fn syntactic_refs_warns_about_attribute_blind_spot() {
    let (env, ok) = run_json(&["refs", "User", "--syntactic"]);
    assert!(ok);
    let warnings = env["warnings"].as_array().expect("warnings present");
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("attribute-access")),
        "syntactic refs should flag the attribute blind spot: {env}"
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

// Regression: an agent reaching for a qualified path (pkg.models.User) should
// resolve by the last component instead of returning a misleading 0.
#[test]
fn dotted_symbol_resolves_by_last_component() {
    let (dotted, ok) = run_json(&["defs", "pkg.models.User"]);
    assert!(ok);
    let (bare, ok) = run_json(&["defs", "User"]);
    assert!(ok);
    assert_eq!(dotted["count"], bare["count"]);
    assert!(dotted["count"].as_u64().unwrap() >= 1);
    // The original qualified input is echoed back in the summary.
    assert!(dotted["summary"].as_str().unwrap().contains("pkg.models.User"));
}

#[test]
fn refs_via_ty_span_multiple_files() {
    let (env, ok) = run_json(&["refs", "User"]);
    assert!(ok);
    // Default is the merged engine, not a ty-only fork.
    assert_eq!(env["query"]["engine"], "unified");
    let files: std::collections::HashSet<_> = locs(&env)
        .iter()
        .map(|l| l.split(':').next().unwrap().to_string())
        .collect();
    // `User` is defined in pkg/models.py and used in app.py — cross-file.
    assert!(files.contains("pkg/models.py"));
    assert!(files.contains("app.py"));
}

// The unification: `defs` is ONE answer with a `role`, not two engines that
// disagree. ty supplies the canonical definition; the syntactic scan supplies
// the `import` binding that re-binds the name, pointed at the canonical def via
// `resolves_to`. An agent filters `role == "definition"` for the origin.
#[test]
fn defs_unified_tags_definition_and_binding() {
    let (env, ok) = run_json(&["defs", "make_user"]);
    assert!(ok);
    assert_eq!(env["query"]["engine"], "unified");
    let results = env["results"].as_array().unwrap();

    let def = results
        .iter()
        .find(|r| r["role"] == "definition")
        .expect("a canonical definition");
    assert_eq!(def["source"], "ty");
    assert!(def["loc"].as_str().unwrap().starts_with("pkg/models.py"));

    let binding = results
        .iter()
        .find(|r| r["role"] == "binding")
        .expect("an import binding");
    assert!(binding["loc"].as_str().unwrap().starts_with("app.py"));
    // The binding resolves to the single canonical definition.
    assert_eq!(binding["resolves_to"], def["loc"]);
}

// Regression (P1 silent-zero): ty cannot see function-local variables and
// returns 0 — which reads as "unused / safe to delete." The merged engine
// fills that blind spot from the syntactic scan and FLAGS it as over-
// approximate, so the count is honest. `admin` is local to app.py's `main`.
#[test]
fn refs_finds_function_local_via_syntactic_fallback() {
    let (env, ok) = run_json(&["refs", "admin"]);
    assert!(ok);
    assert!(
        env["count"].as_u64().unwrap() >= 1,
        "a used local must not report 0: {env}"
    );
    let results = env["results"].as_array().unwrap();
    assert!(
        results.iter().all(|r| r["source"] == "syntactic"),
        "ty is blind to locals, so these come from the syntactic scan: {env}"
    );
    let warnings = env["warnings"].as_array().expect("warnings present");
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("syntactic-only")),
        "over-approximation must be flagged: {env}"
    );
}

// Regression: every call is a reference, so `callers ⊆ refs`. For an aliased
// import (`from pkg.core import make_widget as mw; mw()`), find_references misses
// the call sites under the rename but call_hierarchy follows it — refs must fold
// those in, or an agent reading `refs` concludes an aliased symbol is unused.
#[test]
fn refs_includes_aliased_call_sites_that_callers_finds() {
    let root = fixture("alias");
    let (refs, ok) = run_json_in(&root, &["refs", "make_widget"]);
    assert!(ok);
    let (callers, ok) = run_json_in(&root, &["callers", "make_widget"]);
    assert!(ok);

    let ref_locs: std::collections::HashSet<_> = locs(&refs).into_iter().collect();
    for call in locs(&callers) {
        assert!(ref_locs.contains(&call), "callers ⊆ refs: {call} missing from refs");
    }
    // Both aliased `mw()` call sites are present as references.
    assert!(ref_locs.iter().any(|l| l.starts_with("app.py:5:")));
    assert!(ref_locs.iter().any(|l| l.starts_with("app.py:6:")));
}

#[test]
fn callers_via_ty_finds_the_call_site() {
    let (env, ok) = run_json(&["callers", "make_user"]);
    assert!(ok);
    assert_eq!(env["query"]["kind"], "callers");
    // `make_user("ada")` is called inside app.py's `main`.
    assert!(locs(&env).iter().any(|l| l.starts_with("app.py")));
}

// Regression: a blank symbol is a usage error (exit 1), not a 0-result success
// that an agent would read as "this name is unused."
#[test]
fn empty_symbol_is_a_usage_error() {
    for args in [
        vec!["defs", ""],
        vec!["refs", "   ", "--syntactic"],
        vec!["callers", ""],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
            .args(&args)
            .arg("--root")
            .arg(sample_root())
            .output()
            .expect("pyq should run");
        assert!(
            !out.status.success(),
            "blank symbol should exit non-zero: {args:?}"
        );
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains("symbol must not be empty"), "stderr: {stderr}");
    }
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
    // Directed, closed path notation (not the misleading ↔).
    assert!(label.contains('→'), "cycle should use directed arrows: {label}");
    assert!(!label.contains('↔'));
}

// Regression: imports under `if TYPE_CHECKING:` and inside function bodies are
// not import-time edges, so a mutual import defused that way is NOT a cycle —
// exactly the patterns devs use to break runtime cycles.
#[test]
fn type_checking_and_deferred_imports_are_not_cycles() {
    let (env, ok) = run_json_in(&fixture("typed_cycle"), &["imports", "--cycles"]);
    assert!(ok);
    assert_eq!(
        env["count"].as_u64().unwrap(),
        0,
        "guarded/deferred imports must not count as a cycle: {env}"
    );
}

// Regression: a typo'd module (found:false) must be distinguishable from a real
// leaf with no importers (found:true) — otherwise "0 importers" of a misspelling
// reads as "safe to delete."
#[test]
fn imports_reverse_distinguishes_typo_from_real_leaf() {
    // `config.py` is a real module imported by nobody in the sample.
    let (real, ok) = run_json(&["imports", "config", "--reverse"]);
    assert!(ok);
    assert_eq!(real["query"]["found"], true);
    assert_eq!(real["query"]["target"], "config");
    assert_eq!(real["count"].as_u64().unwrap(), 0);

    // A misspelling is not in the graph at all.
    let (typo, ok) = run_json(&["imports", "config_typo", "--reverse"]);
    assert!(ok);
    assert_eq!(typo["query"]["found"], false);
    assert!(typo["summary"].as_str().unwrap().contains("not found"));
}

// Regression: on a source-rooted layout (code imports app-relative `from
// main.models import X` while the file is alice/main/models.py), forward and
// reverse deps must key on ONE identity. Both the literal import spelling and
// the file-derived id must resolve to the same node — otherwise blast radius
// reads near-zero ("safe to change") when it isn't.
#[test]
fn imports_compose_across_source_root_spellings() {
    let root = fixture("src_root");
    let (literal, ok) = run_json_in(&root, &["imports", "main.models", "--reverse"]);
    assert!(ok);
    let (derived, ok) = run_json_in(&root, &["imports", "alice.main.models", "--reverse"]);
    assert!(ok);

    // Both spellings resolve to the same canonical module and the same importer.
    assert_eq!(literal["query"]["target"], "alice.main.models");
    assert_eq!(derived["query"]["target"], "alice.main.models");
    assert_eq!(literal["count"], derived["count"]);
    assert_eq!(literal["count"].as_u64().unwrap(), 1);
    assert!(locs(&literal).iter().any(|l| l.starts_with("alice/other/views.py")));
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

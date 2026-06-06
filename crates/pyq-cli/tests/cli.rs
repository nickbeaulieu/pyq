//! End-to-end tests: run the built `pyq` binary against `examples/sample` and
//! assert on the JSON envelope. Covers the dispatch wiring, the one resolver
//! contract (locate-then-resolve, no engine exposed), and the shared shape.

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

// The query block is uniform across verbs: kind, target (null where there's no
// single target), and the resolved root — and never an engine name, since the
// caller doesn't choose one.
#[test]
fn query_block_is_uniform_across_verbs() {
    for args in [
        vec!["refs", "User"],
        vec!["defs", "User"],
        vec!["graph", "User"],
        vec!["effects", "User"],
        vec!["tests", "User"],
        vec!["inputs"],
        vec!["imports"],
    ] {
        let (env, ok) = run_json(&args);
        assert!(ok, "{args:?}");
        let q = &env["query"];
        assert!(q.get("kind").is_some(), "kind missing: {args:?}");
        assert!(q.get("target").is_some(), "target key missing (null ok): {args:?}");
        assert!(q.get("root").is_some(), "root missing: {args:?}");
        assert!(q.get("engine").is_none(), "engine must not leak: {args:?}");
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
fn defs_finds_function_and_import_binding() {
    let (env, ok) = run_json(&["defs", "make_user"]);
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

// A qualified path scopes to the def in that module/scope — `pkg.models.User`
// is the class in pkg/models.py, NOT app.py's `import` binding of the same name.
// (A suffix of the scope path matches, so `models.User` works too.)
#[test]
fn qualified_symbol_scopes_to_the_named_def() {
    let (dotted, ok) = run_json(&["defs", "pkg.models.User"]);
    assert!(ok);
    let locs = locs(&dotted);
    assert_eq!(dotted["count"].as_u64().unwrap(), 1, "{dotted}");
    assert!(locs[0].starts_with("pkg/models.py"));
    // The original qualified input is echoed back in the summary.
    assert!(dotted["summary"].as_str().unwrap().contains("pkg.models.User"));

    // A suffix qualifier resolves the same def; a bogus one resolves nothing.
    let (suffix, _) = run_json(&["defs", "models.User"]);
    assert_eq!(suffix["count"].as_u64().unwrap(), 1);
    let (bogus, _) = run_json(&["defs", "Nope.User"]);
    assert_eq!(bogus["count"].as_u64().unwrap(), 0);
}

// Regression: on a source-rooted layout (pyproject `pythonpath = ["src"]`, so
// first-party code imports `helpers.validators` by bare name), a symbol used
// across the bare-path import must still resolve to its def + import + call —
// not silently under-report as ty alone once did (def-only).
#[test]
fn refs_resolve_across_source_root_bare_imports() {
    let (env, ok) = run_json_in(&fixture("pythonpath_root"), &["refs", "valid_email"]);
    assert!(ok);
    let files: std::collections::HashSet<_> = locs(&env)
        .iter()
        .map(|l| l.split(':').next().unwrap().to_string())
        .collect();
    assert!(files.contains("src/helpers/validators.py"), "the def: {env}");
    assert!(files.contains("src/app.py"), "the bare-path use: {env}");
    assert!(env["count"].as_u64().unwrap() >= 3, "{env}");
}

#[test]
fn refs_span_multiple_files() {
    let (env, ok) = run_json(&["refs", "User"]);
    assert!(ok);
    let files: std::collections::HashSet<_> = locs(&env)
        .iter()
        .map(|l| l.split(':').next().unwrap().to_string())
        .collect();
    // `User` is defined in pkg/models.py and used in app.py — cross-file.
    assert!(files.contains("pkg/models.py"));
    assert!(files.contains("app.py"));
}

// `defs` is ONE answer with a `role`. The canonical definition and the `import`
// binding that re-binds the name come back together; the binding points at the
// canonical def via `resolves_to`. An agent filters `role == "definition"`.
#[test]
fn defs_tags_definition_and_binding() {
    let (env, ok) = run_json(&["defs", "make_user"]);
    assert!(ok);
    let results = env["results"].as_array().unwrap();

    let def = results
        .iter()
        .find(|r| r["role"] == "definition")
        .expect("a canonical definition");
    assert!(def["loc"].as_str().unwrap().starts_with("pkg/models.py"));

    let binding = results
        .iter()
        .find(|r| r["role"] == "binding")
        .expect("an import binding");
    assert!(binding["loc"].as_str().unwrap().starts_with("app.py"));
    // The binding resolves to the single canonical definition.
    assert_eq!(binding["resolves_to"], def["loc"]);
}

// Regression (P1 silent-zero): ty cannot see function-local variables on its own
// and would return 0 — which reads as "unused / safe to delete." Locate-then-
// resolve anchors ty at the local's offset, so a used local resolves precisely.
// `admin` is local to app.py's `main`.
#[test]
fn refs_finds_function_local() {
    let (env, ok) = run_json(&["refs", "admin"]);
    assert!(ok);
    assert!(
        env["count"].as_u64().unwrap() >= 1,
        "a used local must not report 0: {env}"
    );
    assert!(locs(&env).iter().all(|l| l.starts_with("app.py")));
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

// Regression: when a bare name has several defs (two classes' `process`), a
// caller query must say which def each site resolves to instead of unioning
// them indistinguishably — otherwise "who calls Alpha.process" wrongly includes
// Beta's callers. Each result carries `resolves_to` the specific def, giving
// per-def targeting by filter.
#[test]
fn callers_disambiguate_same_named_defs_via_resolves_to() {
    let (env, ok) = run_json_in(&fixture("same_name"), &["callers", "process"]);
    assert!(ok);
    let results = env["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);

    let targets: std::collections::HashSet<_> = results
        .iter()
        .map(|r| r["resolves_to"].as_str().expect("each call tagged with its def"))
        .collect();
    // The two call sites resolve to two *different* defs (Alpha vs Beta).
    assert_eq!(targets.len(), 2, "each call should resolve to its own def: {env}");
    // And both point at a real `process` definition in the file.
    assert!(targets.iter().all(|t| t.starts_with("m.py:")));
}

// Qualified targeting: `Alpha.process` scopes to that class's method, so its
// callers are only Alpha's call sites — not Beta's. Bare `process` still unions
// both (each tagged by resolves_to).
#[test]
fn qualified_callers_scope_to_one_def() {
    let root = fixture("same_name");
    let (alpha, ok) = run_json_in(&root, &["callers", "Alpha.process"]);
    assert!(ok);
    assert_eq!(alpha["count"].as_u64().unwrap(), 1, "{alpha}");

    let (beta, ok) = run_json_in(&root, &["callers", "Beta.process"]);
    assert!(ok);
    assert_eq!(beta["count"].as_u64().unwrap(), 1, "{beta}");

    // The two qualified queries resolve to disjoint call sites.
    assert_ne!(locs(&alpha)[0], locs(&beta)[0]);

    // Bare query still unions both methods' callers.
    let (bare, ok) = run_json_in(&root, &["callers", "process"]);
    assert!(ok);
    assert_eq!(bare["count"].as_u64().unwrap(), 2);
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
    for args in [vec!["defs", ""], vec!["refs", "   "], vec!["callers", ""]] {
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
    let (env, ok) = run_json(&["defs", "NoSuchSymbolAnywhere"]);
    assert!(ok, "an unknown symbol should exit 0");
    assert_eq!(env["count"].as_u64().unwrap(), 0);
}

// `graph` is the transitive call-graph primitive (#10): nodes keyed by stable
// FQNs, forward (callees) and reverse (callers) closure. The chain a→b→c proves
// transitivity — `c` is reached from `a` at depth 2, through `b`.
#[test]
fn graph_forward_closure_is_transitive() {
    let root = fixture("callgraph");
    let (env, ok) = run_json_in(&root, &["graph", "a"]);
    assert!(ok);
    assert_eq!(env["query"]["kind"], "graph");
    assert_eq!(env["query"]["mode"], "forward");
    // The symbol resolved to one durable FQN root, echoed in the query.
    assert_eq!(env["query"]["roots"][0], "chain.a");

    let by_fqn: std::collections::HashMap<&str, u64> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["fqn"].as_str().unwrap(), r["depth"].as_u64().unwrap()))
        .collect();
    assert_eq!(by_fqn.get("chain.b"), Some(&1), "direct callee: {env}");
    assert_eq!(by_fqn.get("chain.c"), Some(&2), "transitive callee: {env}");
    // The transitive node names the tree edge it was reached through.
    let c = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["fqn"] == "chain.c")
        .unwrap();
    assert_eq!(c["via"], "chain.b");
}

// Reverse closure is the mirror: everything that transitively *calls* `c`.
#[test]
fn graph_reverse_closure_is_transitive() {
    let root = fixture("callgraph");
    let (env, ok) = run_json_in(&root, &["graph", "c", "--reverse"]);
    assert!(ok);
    assert_eq!(env["query"]["mode"], "reverse");
    let by_fqn: std::collections::HashMap<&str, u64> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["fqn"].as_str().unwrap(), r["depth"].as_u64().unwrap()))
        .collect();
    assert_eq!(by_fqn.get("chain.b"), Some(&1), "direct caller: {env}");
    assert_eq!(by_fqn.get("chain.a"), Some(&2), "transitive caller: {env}");
}

// `--depth N` caps the closure at N hops — the depth-2 node drops out.
#[test]
fn graph_depth_caps_the_closure() {
    let root = fixture("callgraph");
    let (env, ok) = run_json_in(&root, &["graph", "a", "--depth", "1"]);
    assert!(ok);
    let fqns: std::collections::HashSet<&str> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["fqn"].as_str().unwrap())
        .collect();
    assert!(fqns.contains("chain.b"), "depth-1 node kept: {env}");
    assert!(!fqns.contains("chain.c"), "depth-2 node dropped at --depth 1: {env}");
}

// A direct self-recursive function must not hang and reports no *other* reachable
// node (its only callee is itself, already the root).
#[test]
fn graph_handles_recursion_without_looping() {
    let root = fixture("callgraph");
    let (env, ok) = run_json_in(&root, &["graph", "recur"]);
    assert!(ok);
    assert_eq!(env["query"]["roots"][0], "chain.recur");
    assert_eq!(env["count"].as_u64().unwrap(), 0, "{env}");
}

// A full FQN is a durable handle: passing the resolved id back resolves the same
// node (so an agent can re-query after edits without re-grepping line numbers).
#[test]
fn graph_accepts_a_full_fqn_as_a_durable_handle() {
    let (env, ok) = run_json(&["graph", "pkg.models.make_user"]);
    assert!(ok);
    assert_eq!(env["query"]["roots"][0], "pkg.models.make_user");
    // make_user calls the class `User` — one reachable node, fully qualified.
    let fqns: Vec<&str> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["fqn"].as_str().unwrap())
        .collect();
    assert_eq!(fqns, vec!["pkg.models.User"], "{env}");
}

// Cross-file: a forward closure walks through imports. `main` (app.py) reaches
// `make_user` and `User` in pkg/models.py.
#[test]
fn graph_forward_closure_spans_files() {
    let (env, ok) = run_json(&["graph", "main"]);
    assert!(ok);
    let fqns: std::collections::HashSet<&str> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["fqn"].as_str().unwrap())
        .collect();
    assert!(fqns.contains("pkg.models.make_user"), "{env}");
    assert!(fqns.contains("pkg.models.User"), "{env}");
}

// Regression: a function called through an import (`from lib import helper;
// helper()`) must appear in the callee's *reverse* closure. ty's call hierarchy
// from a definition alone misses these cross-module callers — so the reverse
// walk sweeps every occurrence that resolves to the node. Missing them would
// read as "nothing calls this → safe to delete." The disambiguation half: a
// second, unrelated `helper` of the same name in another module must NOT bleed
// its caller into this closure.
#[test]
fn graph_reverse_crosses_imports_without_merging_same_named() {
    let root = fixture("callgraph_import");
    let (env, ok) = run_json_in(&root, &["graph", "lib.helper", "--reverse"]);
    assert!(ok);
    assert_eq!(env["query"]["roots"][0], "lib.helper");
    let fqns: std::collections::HashSet<&str> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["fqn"].as_str().unwrap())
        .collect();
    // The cross-import caller is found …
    assert!(fqns.contains("main.use_lib"), "cross-import caller missing: {env}");
    // … and the same-named `other.helper`'s caller is NOT merged in.
    assert!(
        !fqns.contains("other.use_other"),
        "same-named symbol's caller must not merge: {env}"
    );
    assert_eq!(env["count"].as_u64().unwrap(), 1, "{env}");
}

// A symbol that names no callable is roots-empty with a warning — distinct from
// a real callable that simply reaches nothing (roots-present, count 0). The
// query still exits 0.
#[test]
fn graph_unknown_symbol_warns_and_exits_zero() {
    let (env, ok) = run_json(&["graph", "NoSuchCallable"]);
    assert!(ok, "unknown symbol should exit 0");
    assert_eq!(env["count"].as_u64().unwrap(), 0);
    assert!(env["query"]["roots"].as_array().unwrap().is_empty());
    let warnings = env["warnings"].as_array().expect("a warning");
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("no function or class")),
        "{env}"
    );
}

// `graph` joins the symbol verbs in rejecting a blank symbol (usage error, not a
// 0-result success that reads as "isolated").
#[test]
fn graph_blank_symbol_is_a_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["graph", ""])
        .arg("--root")
        .arg(sample_root())
        .output()
        .expect("pyq should run");
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("symbol must not be empty"), "stderr: {stderr}");
}

// `effects` is the transitive effect surface (#11), a projection of the forward
// call closure. `run` itself touches nothing, but transitively (across files,
// through imports) it hits network, db, and randomness — each site attributed
// to the function that actually performs it.
#[test]
fn effects_aggregates_transitively_across_the_call_closure() {
    let root = fixture("effects");
    let (env, ok) = run_json_in(&root, &["effects", "run"]);
    assert!(ok);
    assert_eq!(env["query"]["kind"], "effects");

    let cats: std::collections::HashSet<&str> = env["query"]["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(cats.contains("network"), "{env}");
    assert!(cats.contains("db"), "{env}");
    assert!(cats.contains("random"), "{env}");

    // The network effect is attributed to io_ops.fetch — the transitive callee
    // that performs it, not to `run`.
    let net = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["effect"] == "network")
        .unwrap();
    assert_eq!(net["owner"], "io_ops.fetch");
    assert_eq!(net["api"], "requests.get");
    assert_eq!(net["import_time"], false);
}

// A genuinely pure function reports no effects, with a clear summary and the
// over-approximation caveat (so "pure" is read as "no effect found", not proof).
#[test]
fn effects_reports_purity_with_a_caveat() {
    let root = fixture("effects");
    let (env, ok) = run_json_in(&root, &["effects", "pure_add"]);
    assert!(ok);
    assert_eq!(env["count"].as_u64().unwrap(), 0);
    assert!(env["summary"].as_str().unwrap().contains("pure"), "{env}");
    assert!(
        env["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("over-approximation")),
        "{env}"
    );
}

// Module-level effectful code is surfaced as import-time: `TOKEN = os.getenv(...)`
// runs when boot.py is imported, and `init` (defined there) is reachable.
#[test]
fn effects_surfaces_import_time_effects() {
    let root = fixture("effects");
    let (env, ok) = run_json_in(&root, &["effects", "init"]);
    assert!(ok);
    let imp = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["import_time"] == true)
        .expect("an import-time effect");
    assert_eq!(imp["effect"], "env");
    assert_eq!(imp["api"], "os.getenv");
}

// A `global` declaration inside a function is a global-state mutation effect.
#[test]
fn effects_flags_global_mutation() {
    let root = fixture("effects");
    let (env, ok) = run_json_in(&root, &["effects", "remember"]);
    assert!(ok);
    let g = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["effect"] == "global")
        .expect("a global-state effect");
    assert_eq!(g["owner"], "io_ops.remember");
}

// `effects` joins the symbol verbs: unknown name → 0 results, warned, exit 0.
#[test]
fn effects_unknown_symbol_warns_and_exits_zero() {
    let (env, ok) = run_json(&["effects", "NoSuchThing"]);
    assert!(ok);
    assert_eq!(env["count"].as_u64().unwrap(), 0);
    assert!(env["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("no function or class")));
}

// `mock-targets` resolves every `mock.patch("a.b.c")` string against the
// project and flags the ones that no longer resolve. The win: a patch into a
// real project module whose looked-up name is gone is *drifted* — it silently
// no-ops, so the test passes while exercising the real code.
#[test]
fn mock_targets_flags_drifted_and_keeps_valid() {
    let root = fixture("mock_targets");
    let (env, ok) = run_json_in(&root, &["mock-targets"]);
    assert!(ok);
    assert_eq!(env["query"]["kind"], "mock-targets");

    // status by target string
    let status: std::collections::HashMap<&str, &str> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| Some((r["target"].as_str()?, r["status"].as_str().unwrap())))
        .collect();

    // Patching an imported name resolves (patch-where-looked-up): client.py
    // does `import requests`, so the binding exists.
    assert_eq!(status.get("myapp.client.requests"), Some(&"valid"));
    // An existing method resolves.
    assert_eq!(status.get("myapp.client.Client.fetch"), Some(&"valid"));
    assert_eq!(status.get("myapp.client.helper"), Some(&"valid"));

    // The two drifted targets: module is ours, the name/member is gone.
    assert_eq!(status.get("myapp.client.deleted_fn"), Some(&"drifted"));
    assert_eq!(status.get("myapp.client.Client.gone"), Some(&"drifted"));

    // Precision: third-party/stdlib and computed targets are NOT flagged broken.
    assert_eq!(status.get("os.path.exists"), Some(&"external"));
    // An attribute on an imported (non-project-class) object is unverifiable,
    // never a false "drifted".
    assert_eq!(status.get("myapp.client.requests.get"), Some(&"unverifiable"));

    // Precision regressions (false positives found on a real Django repo):
    // a builtin patched through the module namespace (`patch("m.open")`) is
    // valid even though `open` is neither defined nor imported there …
    assert_eq!(status.get("myapp.client.open"), Some(&"valid"));
    // … and a missing member on a class that extends a base may be inherited
    // or framework-injected (Django's `objects`/`_save_table`), so it's
    // unverifiable, not a false drift.
    assert_eq!(status.get("myapp.client.Account.injected"), Some(&"unverifiable"));
}

// The drifted targets are elevated to warnings (the actionable signal), and the
// summary counts them; a clean run still exits 0.
#[test]
fn mock_targets_surfaces_drift_as_warnings() {
    let root = fixture("mock_targets");
    let (env, ok) = run_json_in(&root, &["mock-targets"]);
    assert!(ok);
    assert!(env["summary"].as_str().unwrap().contains("2 drifted"), "{env}");
    let warnings = env["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "one warning per drifted target: {env}");
    assert!(warnings
        .iter()
        .any(|w| w.as_str().unwrap().contains("deleted_fn")));
    // The dynamic (computed) target is reported but not a warning.
    let dynamic = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["status"] == "dynamic")
        .expect("the computed target is still reported");
    assert!(dynamic["target"].is_null());
}

// Regression (found running against a nested source-root repo): when code is
// rooted below the repo (files at `alice/main/services.py` but imported and
// patched as `main.services.*`), the patch target's module spelling differs
// from the file-derived id. Without source-root canonicalization every target
// reads as `external` and the tool silently checks nothing. The spelling must
// resolve to the canonical module — so valid patches verify and real drift on
// them still fires.
#[test]
fn mock_targets_resolve_across_source_root_spellings() {
    let root = fixture("mock_src_root");
    let (env, ok) = run_json_in(&root, &["mock-targets"]);
    assert!(ok);
    let status: std::collections::HashMap<&str, &str> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| Some((r["target"].as_str()?, r["status"].as_str().unwrap())))
        .collect();
    // The source-rooted spelling resolves to the real symbol …
    assert_eq!(status.get("main.services.get_thing"), Some(&"valid"), "{env}");
    // … and a drift on that spelling is still caught (not lost to `external`).
    assert_eq!(status.get("main.services.removed_thing"), Some(&"drifted"), "{env}");
}

// Tier-1 third-party resolution: when a patch target's tail attribute is on an
// imported *module*, ty follows the import into typeshed (or site-packages) and
// the attribute is verified there — `time.sleep` is valid, the typo `time.slep`
// is real drift. A symbol binding (`from os import getcwd`) is not a module, so
// `getcwd.x` correctly stays unverifiable rather than being checked wrongly.
#[test]
fn mock_targets_resolve_module_attributes_via_typeshed() {
    let root = fixture("mock_modattr");
    let (env, ok) = run_json_in(&root, &["mock-targets"]);
    assert!(ok);
    let status: std::collections::HashMap<&str, &str> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| Some((r["target"].as_str()?, r["status"].as_str().unwrap())))
        .collect();
    // Resolved into the `time` stub: real attribute is valid, typo is drift.
    assert_eq!(status.get("app.svc.time.sleep"), Some(&"valid"), "{env}");
    assert_eq!(status.get("app.svc.time.slep"), Some(&"drifted"), "{env}");
    // `getcwd` is a function binding, not a module — not checked as a namespace.
    assert_eq!(status.get("app.svc.getcwd.anything"), Some(&"unverifiable"), "{env}");
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

    let (defs, ok) = run_json_in(&root, &["defs", "alpha"]);
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

// ── `tests` verb — the static test↔code map ──────────────────────────────────
// A projection of the reverse call graph filtered to pytest-collected tests.
// `add` is reached directly by `test_add` (depth 1) and transitively, through
// `helper`, by `test_helper` and the `Test*`-class method `test_via_helper`
// (depth 2). The non-test caller `not_a_test` and the non-`test_` method
// `not_collected` must NOT appear — that exclusion is the whole point.
#[test]
fn tests_maps_reaching_tests_transitively() {
    let root = fixture("tests_map");
    let (env, ok) = run_json_in(&root, &["tests", "add"]);
    assert!(ok);
    assert_eq!(env["query"]["kind"], "tests");
    assert_eq!(env["query"]["roots"][0], "pkg.calc.add");

    let by_fqn: std::collections::HashMap<&str, u64> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["fqn"].as_str().unwrap(), r["depth"].as_u64().unwrap()))
        .collect();
    assert_eq!(by_fqn.get("test_calc.test_add"), Some(&1), "direct test: {env}");
    assert_eq!(by_fqn.get("test_calc.test_helper"), Some(&2), "test via helper: {env}");
    assert_eq!(
        by_fqn.get("tests.test_more.TestThings.test_via_helper"),
        Some(&2),
        "Test*-class method via helper: {env}"
    );
    assert!(
        !by_fqn.contains_key("test_calc.not_a_test"),
        "a non-`test_` caller in a test file is not a test: {env}"
    );
    assert!(
        !by_fqn.keys().any(|k| k.contains("not_collected")),
        "a non-`test_` method on a Test* class is not collected: {env}"
    );
    assert!(
        !by_fqn.keys().any(|k| k.contains("use_add")),
        "a caller outside a test file is not a test: {env}"
    );
    // A `*TestCase`-subclassing class with a non-`Test*` name (`AdditionTests`)
    // is collected by pytest via inheritance — its `test_*` method must count,
    // and its `_helper` (non-`test_`) method must not. This is the Django/
    // unittest convention real codebases lean on, missed by a name-only rule.
    assert_eq!(
        by_fqn.get("tests.test_case_subclass.AdditionTests.test_add_is_correct"),
        Some(&1),
        "a TestCase-subclass test method (non-Test* class name) must count: {env}"
    );
    assert!(
        !by_fqn.contains_key("tests.test_case_subclass.AdditionTests._helper"),
        "a non-`test_` method on a TestCase subclass is not collected: {env}"
    );
}

// A symbol reached by no test is a 0-result success (distinct summary), NOT the
// "not found" case — an agent must tell "exists but untested" from "no such
// symbol", since the first is a coverage gap and the second is a typo.
#[test]
fn tests_distinguishes_untested_from_not_found() {
    let root = fixture("tests_map");

    let (untested, ok) = run_json_in(&root, &["tests", "use_add"]);
    assert!(ok);
    assert_eq!(untested["query"]["roots"][0], "uses_calc.use_add");
    assert_eq!(untested["count"].as_u64().unwrap(), 0, "{untested}");
    assert!(
        untested["summary"].as_str().unwrap().contains("no test"),
        "untested summary: {untested}"
    );

    let (missing, ok) = run_json_in(&root, &["tests", "nope"]);
    assert!(ok);
    assert!(missing["query"]["roots"].as_array().unwrap().is_empty(), "{missing}");
    assert!(
        missing["summary"].as_str().unwrap().contains("found"),
        "not-found summary: {missing}"
    );
    assert!(
        missing["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("no function or class")),
        "{missing}"
    );
}

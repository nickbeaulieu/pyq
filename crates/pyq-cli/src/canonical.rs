//! The `canonical` verb — the repo's most-used helpers, untested public
//! surface, and test inventory, in one pass.
//!
//! Where the symbol verbs answer "tell me about *X*," `canonical` answers "tell
//! me about *the repo*" — the three project-wide facts an agent wants before it
//! starts editing an unfamiliar codebase, packed into one envelope, each row
//! tagged with its `section`:
//!
//!   - **`most-used`** — internal callables ranked by use, surfacing the
//!     utilities to reach for rather than reinvent. "Use" is the count of
//!     *distinct callers defined outside the test tree* — a projection of the
//!     call graph's in-degree ([`CallGraph::caller_index`]). A helper used in
//!     ≥2 non-test places is shown; the top [`MAX_MOST_USED`] by use are kept.
//!     Candidates defined in the test tree or an entrypoint file (`scripts/`,
//!     `manage.py`, migrations, …) are excluded — a fixture or a one-off script
//!     helper isn't a reusable utility to reach for.
//!   - **`untested-public`** — the public surface (top-level, non-`_` functions
//!     and classes) that *no collected test statically reaches* and that the
//!     framework doesn't drive. The same reachability machinery as `deadcode`,
//!     seeded from the test set instead of the entrypoints; then framework
//!     entrypoints (serializers, configs, commands, decorated handlers,
//!     string-config targets — `deadcode::framework_entry_fqns`) are subtracted,
//!     since a test rarely *calls* them and leaving them in would bury the real
//!     gaps under framework classes exercised through dispatch.
//!   - **`test`** — the test inventory: every pytest-collected test, with the
//!     markers read off its (and its class's) decorators.
//!
//! Same boundary as the rest of the graph-backed verbs, and it cuts both ways
//! here: a helper reached only through attribute/framework dispatch is
//! *undercounted* in `most-used`, and a public symbol exercised only through
//! dynamic dispatch (a fixture, a route, a signal) is *falsely* flagged
//! `untested-public`. So "untested" means "no *static* reaching test," not
//! "uncovered" — `change-coverage` (#9.4) is the runtime oracle there.

use crate::{deadcode, hierarchy, plural, tests_map, walk};
use pyq_index::{Def, DefKind, FileIndex};
use pyq_output::Envelope;
use pyq_resolve::{scope_fqn, CallGraph};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// How many most-used helpers to surface — a ranking, kept token-frugal. The
/// cut is flagged in a warning when it bites.
const MAX_MOST_USED: usize = 30;

/// The minimum distinct non-test callers for a helper to rank — used in at
/// least two places outside the test tree, not a one-off.
const MOST_USED_FLOOR: usize = 2;

/// Build the canonical overview of the project at `root`.
pub fn query(root: &str) -> anyhow::Result<Envelope> {
    let files = walk::index_tree(root)?;
    let scope = walk::walked_py_files(root);
    let test_classes = tests_map::test_class_fqns(&files);

    // FQN → (location, node kind) for every first-party callable, so a most-used
    // FQN (from the caller index) maps back to a row.
    let mut info: HashMap<String, (String, &'static str)> = HashMap::new();
    // The FQNs of *every* callable defined in the test tree — collected tests,
    // the test classes' other methods, fixtures, factories, conftest helpers.
    // Used to exclude test-support code from the most-used ranking (both as
    // candidate and as a caller): a helper only test code uses isn't a
    // production utility.
    let mut test_tree_fqns: HashSet<String> = HashSet::new();
    // FQNs defined in an entrypoint file (`scripts/`, `manage.py`, `urls.py`,
    // migrations, management commands). Excluded as most-used *candidates* —
    // glue, not a reusable utility — but they still count as callers (a script
    // exercising a helper is real use).
    let mut entrypoint_fqns: HashSet<String> = HashSet::new();
    for f in &files {
        let in_test = in_test_tree(&f.path);
        let entry_file = deadcode::is_entrypoint_file(&f.path);
        for d in &f.defs {
            if !matches!(d.kind, DefKind::Function | DefKind::Class) {
                continue;
            }
            let fqn = scope_fqn(&f.path, &def_scope(d));
            let loc = format!("{}:{}:{}", f.path, d.pos.line, d.pos.col);
            info.insert(fqn.clone(), (loc, node_kind(d)));
            if in_test {
                test_tree_fqns.insert(fqn.clone());
            }
            if entry_file {
                entrypoint_fqns.insert(fqn);
            }
        }
    }

    let graph = CallGraph::new(root, files.clone(), scope)?;
    let hier = hierarchy::Hierarchy::build(&files, &graph);

    let most_used = most_used_rows(&graph, &info, &test_tree_fqns, &entrypoint_fqns);
    let most_used_truncated = most_used.len() > MAX_MOST_USED;
    let untested = untested_public_rows(root, &files, &graph, &hier, &test_classes);
    let tests = test_inventory_rows(&files, &test_classes);

    let n_most_used = most_used.len().min(MAX_MOST_USED);
    let n_untested = untested.len();
    let n_tests = tests.len();

    let mut results: Vec<Value> = Vec::new();
    results.extend(most_used.into_iter().take(MAX_MOST_USED));
    results.extend(untested);
    results.extend(tests);

    let summary = format!(
        "canonical: {n_most_used} most-used {}, {n_untested} untested public {}, {n_tests} collected {}",
        plural(n_most_used, "helper"),
        plural(n_untested, "symbol"),
        plural(n_tests, "test"),
    );

    let mut warnings = vec![
        format!(
            "most-used = ranked by distinct callers defined outside the test tree (≥{MOST_USED_FLOOR} \
             shown); calls made through dynamic/attribute dispatch are not counted, so a helper \
             invoked only reflectively is undercounted."
        ),
        "untested-public = a top-level public (non-`_`) function/class no collected test \
         statically reaches, with framework-driven symbols excluded (serializers, configs, \
         migrations, commands, routers, decorated handlers, string-config targets — run through \
         dispatch, not a direct test call). Other dynamic reach is still invisible, so a flagged \
         symbol may yet be exercised at runtime (`change-coverage` is the oracle, not this)."
            .to_string(),
        "test collection uses pytest naming + unittest/TestCase-inheritance rules (custom \
         python_files/python_classes config not read); markers are read from decorators on the \
         test and its enclosing class — module-level `pytestmark` is not captured."
            .to_string(),
    ];
    if most_used_truncated {
        warnings.push(format!(
            "most-used list truncated to the top {MAX_MOST_USED} by use"
        ));
    }

    Ok(Envelope::new(json!({ "kind": "canonical", "target": null }), results)
        .with_summary(summary)
        .with_warnings(warnings))
}

/// The most-used rows: first-party callables ranked by distinct non-test-tree
/// caller count (descending), floored at [`MOST_USED_FLOOR`]. Candidates defined
/// in the test tree or an entrypoint file (`scripts/`, `manage.py`, migrations,
/// …), and dunder methods, are excluded (a fixture, a one-off script helper, or
/// a dunder isn't a reusable utility to reach for). Test-tree callers don't
/// count toward the total; entrypoint-file callers still do (a script
/// exercising a helper is real use).
fn most_used_rows(
    graph: &CallGraph,
    info: &HashMap<String, (String, &'static str)>,
    test_tree_fqns: &HashSet<String>,
    entrypoint_fqns: &HashSet<String>,
) -> Vec<Value> {
    let caller_index = graph.caller_index();
    let mut ranked: Vec<(String, usize)> = caller_index
        .into_iter()
        .filter(|(fqn, _)| {
            !test_tree_fqns.contains(fqn) && !entrypoint_fqns.contains(fqn) && !leaf_is_dunder(fqn)
        })
        .map(|(fqn, callers)| {
            let uses = callers.iter().filter(|c| !test_tree_fqns.contains(*c)).count();
            (fqn, uses)
        })
        .filter(|(_, uses)| *uses >= MOST_USED_FLOOR)
        .collect();
    // Most-used first; ties broken by FQN for a stable order.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    ranked
        .into_iter()
        .filter_map(|(fqn, uses)| {
            let (loc, kind) = info.get(&fqn)?;
            Some(json!({
                "loc": loc,
                "label": format!("most-used {fqn} (used by {uses})"),
                "section": "most-used",
                "fqn": fqn,
                "node_kind": kind,
                "uses": uses,
                "group": "most-used",
                "cols": [fqn.clone(), format!("used by {uses}")],
            }))
        })
        .collect()
}

/// The untested-public rows: top-level public functions/classes that no
/// collected test reaches *and* that the framework doesn't drive. Reachability
/// mirrors `deadcode` — seeded from the test defs, with the same override edges
/// so a public symbol reached only polymorphically from a test still counts as
/// tested (no false "untested"). On top of that, symbols the framework enters
/// without a direct call (serializers, configs, migrations, commands, routers,
/// decorated handlers, string-config targets) are subtracted: a test rarely
/// *calls* them, so leaving them in would swamp the list with framework classes
/// that are in fact exercised through dispatch — `deadcode::framework_entry_fqns`
/// is the same signal that keeps them out of the dead-code report.
fn untested_public_rows(
    root: &str,
    files: &[FileIndex],
    graph: &CallGraph,
    hier: &hierarchy::Hierarchy,
    test_classes: &HashSet<String>,
) -> Vec<Value> {
    // Seed the reachability walk from every collected test.
    let mut seeds: Vec<(String, u32)> = Vec::new();
    for f in files {
        for d in &f.defs {
            if tests_map::is_collected_test_def(d, &f.path, test_classes) {
                seeds.push((f.path.clone(), d.offset));
            }
        }
    }
    let def_anchor = deadcode::def_anchors(files);
    let override_edges = deadcode::override_edges(hier, &def_anchor);
    let reached = graph.reachable_from(&seeds, &override_edges);
    // What the framework drives — never a "you forgot to test this" finding.
    let framework = deadcode::framework_entry_fqns(files, graph, hier, root);
    // A class FQN that itself never gets reached may still be tested through one
    // of its methods — collect the classes any reached method belongs to so an
    // instance-method-only test still counts the class as covered.
    let reached_owners: HashSet<&str> = reached
        .iter()
        .filter_map(|fqn| fqn.rsplit_once('.').map(|(owner, _)| owner))
        .collect();

    let mut rows: Vec<(String, Value)> = Vec::new();
    for f in files {
        // A public symbol in the test tree isn't the project's public API.
        if in_test_tree(&f.path) {
            continue;
        }
        for d in &f.defs {
            if !is_public_surface(d) {
                continue;
            }
            let fqn = scope_fqn(&f.path, &def_scope(d));
            let tested = reached.contains(&fqn)
                || (d.kind == DefKind::Class && reached_owners.contains(fqn.as_str()));
            // Skip framework-driven symbols: exercised through dispatch, not a
            // direct test call, so flagging them "untested" is a false alarm.
            if tested || framework.contains(&fqn) {
                continue;
            }
            let loc = format!("{}:{}:{}", f.path, d.pos.line, d.pos.col);
            rows.push((
                loc.clone(),
                json!({
                    "loc": loc,
                    "label": format!("untested-public {fqn}"),
                    "section": "untested-public",
                    "fqn": fqn,
                    "node_kind": node_kind(d),
                    "group": "untested-public",
                    "cols": [fqn.clone()],
                }),
            ));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, v)| v).collect()
}

/// The test-inventory rows: every collected test, with the pytest markers read
/// from its own decorators and those of its enclosing class (class-level marks
/// apply to every test method). One row per test def, ordered by FQN.
fn test_inventory_rows(files: &[FileIndex], test_classes: &HashSet<String>) -> Vec<Value> {
    let mut rows: Vec<(String, Value)> = Vec::new();
    for f in files {
        if !tests_map::is_test_file(&f.path) {
            continue;
        }
        // Class FQN → its decorator-borne markers, so a method can inherit them.
        let class_markers: HashMap<String, Vec<String>> = f
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Class)
            .map(|d| (scope_fqn(&f.path, &def_scope(d)), pytest_markers(&d.decorators)))
            .collect();

        for d in &f.defs {
            if !tests_map::is_collected_test_def(d, &f.path, test_classes) {
                continue;
            }
            let fqn = scope_fqn(&f.path, &def_scope(d));
            let mut markers = pytest_markers(&d.decorators);
            // A method inherits its class's marks.
            if !d.container.is_empty() {
                if let Some(cm) = class_markers.get(&scope_fqn(&f.path, &d.container)) {
                    markers.extend(cm.iter().cloned());
                }
            }
            markers.sort();
            markers.dedup();
            let suffix = if markers.is_empty() {
                String::new()
            } else {
                format!(" [{}]", markers.join(", "))
            };
            let loc = format!("{}:{}:{}", f.path, d.pos.line, d.pos.col);
            let cols: Vec<String> = if markers.is_empty() {
                vec![fqn.clone()]
            } else {
                vec![fqn.clone(), format!("[{}]", markers.join(", "))]
            };
            rows.push((
                fqn.clone(),
                json!({
                    "loc": loc,
                    "label": format!("test {fqn}{suffix}"),
                    "section": "test",
                    "fqn": fqn,
                    "markers": markers,
                    "group": "tests",
                    "cols": cols,
                }),
            ));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, v)| v).collect()
}

/// The pytest marker names carried by a decorator list: the segment after a
/// `pytest.mark.` / `mark.` prefix, up to the call parens or end
/// (`pytest.mark.parametrize(...)` → `parametrize`, `pytest.mark.slow` →
/// `slow`). A `.mark.` must sit on a dot boundary so `app.markup.x` doesn't
/// match. Non-marker decorators (`staticmethod`, `app.route(...)`) yield none.
fn pytest_markers(decorators: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for d in decorators {
        let Some(idx) = d.find("mark.") else { continue };
        // `mark.` must be at the start or preceded by a `.` (so it's the pytest
        // `mark` namespace, not the tail of another identifier like `markup`).
        if idx != 0 && d.as_bytes()[idx - 1] != b'.' {
            continue;
        }
        let name: String = d[idx + "mark.".len()..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// Whether a path is part of the test tree — broader than pytest's collection
/// rule ([`tests_map::is_test_file`]): any file under a `tests`/`test`
/// directory, plus `conftest.py`. Test-support modules (`tests/factories.py`,
/// `tests/utils.py`, `conftest.py`) hold fixtures and factories, not production
/// utilities — so the most-used ranking and the public surface both exclude
/// them, even though pytest doesn't *collect* them as tests (the inventory
/// keeps the strict rule).
fn in_test_tree(path: &str) -> bool {
    if tests_map::is_test_file(path) {
        return true;
    }
    let comps: Vec<&str> = path.split(['/', '\\']).collect();
    comps.iter().any(|c| *c == "tests" || *c == "test")
        || comps.last() == Some(&"conftest.py")
}

/// Whether a def is part of the project's public surface: a top-level
/// (module-scope) function or class whose name isn't `_`-prefixed.
fn is_public_surface(d: &Def) -> bool {
    matches!(d.kind, DefKind::Function | DefKind::Class)
        && d.container.is_empty()
        && !d.name.starts_with('_')
}

/// A def's full scope path (enclosing scopes + its own name).
fn def_scope(d: &Def) -> Vec<String> {
    let mut s = d.container.clone();
    s.push(d.name.clone());
    s
}

/// The display kind of a callable def: a class, a free function, or a method
/// (a function nested in a class/function).
fn node_kind(d: &Def) -> &'static str {
    match d.kind {
        DefKind::Class => "class",
        _ if d.container.is_empty() => "function",
        _ => "method",
    }
}

/// Whether an FQN's leaf name is a dunder (`__init__`, `__enter__`) — excluded
/// from the most-used ranking as runtime plumbing rather than a reusable helper.
fn leaf_is_dunder(fqn: &str) -> bool {
    let leaf = fqn.rsplit('.').next().unwrap_or(fqn);
    leaf.starts_with("__") && leaf.ends_with("__") && leaf.len() > 4
}

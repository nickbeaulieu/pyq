//! The `tests` verb — the static test↔code map.
//!
//! "Which tests are call-wired to this symbol" is the question an iterating
//! agent asks *before* an edit — which tests to run, what might break — and this
//! answers it as a structural fact, not a coverage measurement. It's a
//! projection of the [`CallGraph`] reverse closure (#10): the set of callables
//! that transitively reach the queried symbol, filtered to the ones a runner
//! would *collect as tests*. Each reaching test carries the `via` edge and
//! `depth`, so an agent sees the call path, not just the fact.
//!
//! This is a *call-reachability lens, not a coverage metric.* Reach through
//! dynamic dispatch (attribute calls, framework routing, signals/Celery,
//! `getattr`) is invisible — the same boundary `callers`/`graph --reverse` have —
//! so a framework-dispatched view or signal handler can show zero reaching tests
//! while being fully exercised at runtime. A `0` means "no *static* reaching
//! test found," never "untested"; `coverage.py` is the oracle for the dynamic
//! paths. Aggregating this into a coverage percentage will mislead.
//!
//! Test collection follows pytest's defaults *and* the unittest/Django rule a
//! Python-heavy codebase actually leans on: a test lives in a file matching
//! `test_*.py` / `*_test.py`, is a function whose name starts with `test`, and —
//! for a method — sits inside a class pytest would collect. A class is collected
//! when its name matches the `Test*` convention **or** it subclasses a
//! `*TestCase` (`unittest.TestCase`, Django's `TestCase`/`SimpleTestCase`/
//! `TransactionTestCase`, DRF's `APITestCase`, …) — pytest collects `TestCase`
//! subclasses by inheritance regardless of name, and `*Tests`-suffixed Django
//! test classes are ubiquitous. Custom `python_files`/`python_classes` overrides
//! in `pytest.ini`/`pyproject.toml` are not read — noted as a boundary, not
//! silently assumed away.

use crate::plural;
use pyq_index::{DefKind, FileIndex};
use pyq_resolve::{scope_fqn, GraphNode};
use std::collections::HashSet;
use std::path::Path;

/// Whether a file path is one pytest would collect tests from.
pub fn is_test_file(path: &str) -> bool {
    let base = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    base.starts_with("test_") || base.ends_with("_test.py")
}

/// Whether a function's bare name is one pytest collects (`test`-prefixed).
fn is_test_name(name: &str) -> bool {
    name.starts_with("test")
}

/// The FQNs of every class pytest would collect as a test class: name matches
/// `Test*`, or it subclasses a `*TestCase` (unittest/Django/DRF). Built once from
/// the index and consulted by [`is_test_node`] for method nodes — the call graph
/// node alone carries no base-class info, so we resolve the enclosing class's
/// collectability here, where the `Def::bases` the syntactic index recorded live.
pub fn test_class_fqns(files: &[FileIndex]) -> HashSet<String> {
    let mut out = HashSet::new();
    for f in files {
        if !is_test_file(&f.path) {
            continue;
        }
        for d in &f.defs {
            if d.kind != DefKind::Class {
                continue;
            }
            let collected = d.name.starts_with("Test")
                || d.bases.iter().any(|b| base_is_test_case(b));
            if collected {
                let mut scope = d.container.clone();
                scope.push(d.name.clone());
                out.insert(scope_fqn(&f.path, &scope));
            }
        }
    }
    out
}

/// Whether a base-class dotted name marks a unittest-style test case — the
/// inheritance pytest collects regardless of class name (`TestCase`,
/// `SimpleTestCase`, `TransactionTestCase`, `APITestCase`, `LiveServerTestCase`,
/// `IsolatedAsyncioTestCase`, …). Suffix-based so it follows the import spelling
/// (`unittest.TestCase`, `django.test.TestCase`, bare `TestCase`).
fn base_is_test_case(base: &str) -> bool {
    let leaf = base.rsplit('.').next().unwrap_or(base);
    leaf.ends_with("TestCase")
}

/// Decide whether a call-graph node is a pytest-collected test, given its FQN
/// and ty kind. The FQN's leaf is the function name; the segment before it (for
/// a method) is the enclosing class. A module-scope `test_*` function, or a
/// `test_*` method whose enclosing class is in `test_classes`, qualifies.
pub fn is_test_node(node: &GraphNode, test_classes: &HashSet<String>) -> bool {
    if !is_test_file(&node.path) {
        return false;
    }
    if !matches!(node.kind, "function" | "method") {
        return false;
    }
    let Some((class_fqn, leaf)) = node.fqn.rsplit_once('.') else {
        return is_test_name(&node.fqn);
    };
    if !is_test_name(leaf) {
        return false;
    }
    // A method must sit inside a class pytest collects; a free function need only
    // be `test`-prefixed in a test file.
    match node.kind {
        "method" => test_classes.contains(class_fqn),
        _ => true,
    }
}

/// A human summary line for the `tests <symbol>` result set.
pub fn summary(symbol: &str, roots_empty: bool, n: usize) -> String {
    if roots_empty {
        format!("no function or class named `{symbol}` found")
    } else if n == 0 {
        format!("no test statically reaches `{symbol}`")
    } else {
        format!("{n} {} reach `{symbol}`", plural(n, "test"))
    }
}

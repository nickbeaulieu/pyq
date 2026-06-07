//! End-to-end tests for the `canonical` verb (#20) over the built `pyq` binary.
//!
//! `canonical` packs three project-wide facets into one envelope, each row
//! tagged with its `section` — so these assert on the rows by `section`, not
//! position. The fixture is built so each facet has an unambiguous expected
//! answer: one helper used by three production callers (`most-used`), a public
//! surface that is partly test-reached and partly not (`untested-public`), and
//! a handful of collected tests carrying pytest markers (`test`).

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

/// Absolute path to the committed `canonical` fixture.
fn canonical_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/canonical")
        .canonicalize()
        .expect("canonical fixture should exist")
}

/// Run `pyq canonical --root <fixture> --json` and parse the envelope.
fn canonical(root: &std::path::Path) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(root)
        .arg("canonical")
        .output()
        .expect("pyq should run");
    assert!(out.status.success(), "canonical should exit 0");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("envelope should be JSON: {e}\n{}", String::from_utf8_lossy(&out.stdout))
    })
}

/// Every `fqn` among rows of the given `section`.
fn fqns_in(env: &Value, section: &str) -> HashSet<String> {
    env["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["section"] == section)
        .map(|r| r["fqn"].as_str().unwrap().to_string())
        .collect()
}

// The headline: most-used helpers ranked by use, the untested public surface,
// and the marker-bearing test inventory all come back in one envelope, each
// tagged by `section`.
#[test]
fn canonical_packs_most_used_untested_public_and_test_inventory() {
    let env = canonical(&canonical_root());
    assert_eq!(env["tool"], "pyq");
    assert_eq!(env["query"]["kind"], "canonical");
    assert!(env["query"].get("root").is_some());
    assert!(env["query"].get("target").is_some(), "target key present (null ok)");

    // --- most-used: `normalize` has three production callers; nothing else
    // clears the floor. A helper called only by tests (`tested_public`) does NOT
    // rank, and a private name with a single caller never does.
    let most_used: HashMap<String, u64> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["section"] == "most-used")
        .map(|r| (r["fqn"].as_str().unwrap().to_string(), r["uses"].as_u64().unwrap()))
        .collect();
    assert_eq!(most_used.get("pkg.core.normalize"), Some(&3), "{env}");
    assert!(!most_used.contains_key("pkg.core.tested_public"), "test-only callers don't rank: {env}");
    assert!(!most_used.contains_key("pkg.core.parse_name"), "a single caller is below the floor: {env}");
    assert!(!most_used.keys().any(|k| k.ends_with("_private")), "{env}");

    // --- untested-public: the public symbols no test reaches. `parse_title`,
    // `parse_tag`, `untested_public` are out of the tests' forward closure …
    let untested = fqns_in(&env, "untested-public");
    for fqn in ["pkg.core.parse_title", "pkg.core.parse_tag", "pkg.core.untested_public"] {
        assert!(untested.contains(fqn), "`{fqn}` should be untested-public: {untested:?}");
    }
    // … while `tested_public` (reached directly), `parse_name` and `normalize`
    // (reached transitively) are excluded — that exclusion is the whole point.
    for fqn in ["pkg.core.tested_public", "pkg.core.parse_name", "pkg.core.normalize"] {
        assert!(!untested.contains(fqn), "`{fqn}` is test-reached, not untested: {untested:?}");
    }
    // Private symbols and test-file defs are never part of the public surface.
    assert!(!untested.contains("pkg.core._private"), "private excluded: {untested:?}");
    assert!(!untested.iter().any(|f| f.starts_with("tests.")), "test-file defs excluded: {untested:?}");
    // Framework-aware: a DRF serializer (external base, framework-driven) and a
    // decorated handler are exercised through dispatch, not a direct test call —
    // they must NOT swamp the list as false "untested," even though no test
    // reaches them statically.
    assert!(!untested.contains("pkg.api.WidgetSerializer"), "framework class excluded: {untested:?}");
    assert!(!untested.contains("pkg.api.cached_handler"), "decorated handler excluded: {untested:?}");

    // --- test inventory: every collected test, with markers off its (and its
    // class's) decorators. The non-`test_*` siblings are not collected.
    let markers: HashMap<String, Vec<String>> = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["section"] == "test")
        .map(|r| {
            let ms = r["markers"].as_array().unwrap().iter().map(|m| m.as_str().unwrap().to_string()).collect();
            (r["fqn"].as_str().unwrap().to_string(), ms)
        })
        .collect();
    assert_eq!(markers.get("tests.test_core.test_tested_public"), Some(&vec!["slow".to_string()]), "{env}");
    assert_eq!(markers.get("tests.test_core.test_param"), Some(&vec!["parametrize".to_string()]), "{env}");
    // A class-level mark is inherited by the method.
    assert_eq!(
        markers.get("tests.test_models.TestModels.test_one"),
        Some(&vec!["django_db".to_string()]),
        "class-level marker inherited by the method: {env}"
    );
    assert!(!markers.keys().any(|k| k.ends_with("not_a_test")), "a non-`test_*` function isn't collected: {env}");
    assert!(!markers.keys().any(|k| k.ends_with("helper")), "a non-`test_*` method isn't collected: {env}");
}

// `canonical` always carries its three caveats (the static blind spot cuts both
// ways — undercounts most-used, over-reports untested-public) so a consumer
// never reads "untested" as "uncovered".
#[test]
fn canonical_flags_its_static_boundaries() {
    let env = canonical(&canonical_root());
    let warnings: Vec<String> = env["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect();
    assert!(warnings.iter().any(|w| w.contains("untested-public")), "{warnings:?}");
    assert!(warnings.iter().any(|w| w.contains("dynamic")), "{warnings:?}");
    assert!(warnings.iter().any(|w| w.contains("pytestmark")), "{warnings:?}");
}

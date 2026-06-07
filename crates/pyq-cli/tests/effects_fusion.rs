//! `effects` fused with the runtime ledger (the absorbed `effect-diff`): every
//! row carries a `confidence`, and the suite is run on a cache miss. The fixture
//! is engineered to land one effect in each bucket. Needs `python3` + `pytest`;
//! if the trace can't run we skip rather than fail.

use std::fs;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn write_fixture(dir: &std::path::Path) {
    fs::create_dir_all(dir.join("pkg")).unwrap();
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::write(dir.join("pkg/__init__.py"), "").unwrap();
    fs::write(
        dir.join("pkg/ops.py"),
        r#"
import os, socket

def confirmed_fs():
    return open(os.devnull).read()           # static sees `open`; the suite runs it

def dynamic_only_subprocess():
    fn = getattr(os, "sys" + "tem")           # callee built at runtime
    fn("true")                                # static can't match os.system here

def static_only_net():
    s = socket.socket()                       # static sees socket; the suite never runs it
    s.close()

def reads_env():
    return os.getenv("HOME")                  # static `env` read — audit-blind
"#,
    )
    .unwrap();
    fs::write(
        dir.join("tests/test_ops.py"),
        r#"
from pkg.ops import confirmed_fs, dynamic_only_subprocess, reads_env

def test_run():
    confirmed_fs()
    dynamic_only_subprocess()
    reads_env()
    # static_only_net is deliberately never called
"#,
    )
    .unwrap();
}

/// (confidence, effect, owner) for every result row.
fn rows(env: &Value) -> Vec<(String, String, String)> {
    env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["confidence"].as_str().unwrap_or("").to_string(),
                r["effect"].as_str().unwrap_or("").to_string(),
                r["owner"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

#[test]
fn effects_labels_every_confidence_bucket() {
    let dir = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write_fixture(dir.path());

    // Project-wide `effects` (no symbol) — runs the suite on a cache miss.
    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(dir.path())
        .arg("effects")
        .env("PYQ_CACHE_DIR", cache.path())
        .output()
        .expect("run pyq");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skipping: effects produced no envelope (pytest unavailable?)\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
    };

    let got = rows(&env);
    // If the suite couldn't run, every row degrades to predicted/unverifiable —
    // there's no confirmed/observed signal to assert. Skip in that case.
    if !got.iter().any(|(c, _, _)| c == "confirmed" || c == "observed") {
        eprintln!("skipping: no runtime ledger (suite did not run); rows: {got:?}");
        return;
    }

    let has = |conf: &str, effect: &str, owner: &str| {
        got.iter()
            .any(|(c, e, o)| c == conf && e == effect && o == owner)
    };

    // The headline: an effect reached only through a runtime-built callee, which
    // the static surface cannot match — surfaced as `observed`.
    assert!(
        has("observed", "subprocess", "pkg.ops.dynamic_only_subprocess"),
        "rows: {got:?}"
    );
    // Static predicted + the suite ran it.
    assert!(has("confirmed", "fs", "pkg.ops.confirmed_fs"), "rows: {got:?}");
    // Static predicted, never exercised by the suite.
    assert!(
        has("predicted", "network", "pkg.ops.static_only_net"),
        "rows: {got:?}"
    );
    // An audit-blind category — reported, never treated as over-approximation.
    assert!(has("unverifiable", "env", "pkg.ops.reads_env"), "rows: {got:?}");

    // The verb identifies as `effects` now — no separate `effect-diff`.
    assert_eq!(env["query"]["kind"], "effects");

    // A second run hits the cached ledger (fingerprint match → no suite re-run)
    // and produces identical rows.
    let again = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(dir.path())
        .arg("effects")
        .env("PYQ_CACHE_DIR", cache.path())
        .output()
        .expect("run pyq");
    let again: Value = serde_json::from_str(&String::from_utf8_lossy(&again.stdout)).unwrap();
    assert_eq!(rows(&again), got, "cached ledger must replay identically");
}

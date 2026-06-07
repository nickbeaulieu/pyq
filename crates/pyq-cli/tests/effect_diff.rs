//! effect-diff (#9.3) over the built `pyq` binary: assert each bucket on a
//! fixture engineered to land one effect in each. Needs `python3` + `pytest`;
//! if the trace can't run we skip rather than fail.

use std::fs;
use std::process::Command;

use serde_json::Value;

fn write_fixture(dir: &std::path::Path) {
    fs::create_dir_all(dir.join("pkg")).unwrap();
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::write(dir.join("pkg/__init__.py"), "").unwrap();
    fs::write(
        dir.join("pkg/ops.py"),
        r#"
import os, socket

def confirmed_fs():
    return open(os.devnull).read()           # static sees `open`; suite runs it

def dynamic_only_subprocess():
    fn = getattr(os, "sys" + "tem")           # callee built at runtime
    fn("true")                                # static can't match os.system here

def static_only_net():
    s = socket.socket()                       # static sees socket; suite never runs it
    s.close()

def reads_env():
    return os.getenv("HOME")                  # static `env` read — unauditable
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

/// (status, effect, owner) for every result row.
fn rows(env: &Value) -> Vec<(String, String, String)> {
    env["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["status"].as_str().unwrap_or("").to_string(),
                r["effect"].as_str().unwrap_or("").to_string(),
                r["owner"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

#[test]
fn effect_diff_buckets() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(dir.path())
        .args(["effect-diff", "-q"])
        .output()
        .expect("run pyq");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skipping: pyq trace produced no envelope (pytest unavailable?)\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
    };

    let rows = rows(&env);
    let has = |status: &str, effect: &str, owner: &str| {
        rows.iter()
            .any(|(s, e, o)| s == status && e == effect && o == owner)
    };

    // The headline: an effect reached only through a runtime-built callee, which
    // the syntactic static surface cannot match.
    assert!(
        has("dynamic-only", "subprocess", "pkg.ops.dynamic_only_subprocess"),
        "rows: {rows:?}"
    );
    // Predicted and observed.
    assert!(has("confirmed", "fs", "pkg.ops.confirmed_fs"), "rows: {rows:?}");
    // Predicted, never exercised by the suite.
    assert!(
        has("static-only", "network", "pkg.ops.static_only_net"),
        "rows: {rows:?}"
    );
    // An unaudited category — reported but not treated as over-approximation.
    assert!(has("unverifiable", "env", "pkg.ops.reads_env"), "rows: {rows:?}");
}

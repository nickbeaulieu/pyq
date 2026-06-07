//! The `shapes` fold: `describe` shows the runtime-observed return type next to
//! the declared signature (`observed_return`), running the suite on a cache miss.
//! Needs python3 + pytest (3.12+ for real shapes); skips otherwise.

use std::fs;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

#[test]
fn describe_shows_observed_return_type() {
    let dir = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let p = dir.path();
    fs::create_dir_all(p.join("pkg")).unwrap();
    fs::create_dir_all(p.join("tests")).unwrap();
    fs::write(p.join("pkg/__init__.py"), "").unwrap();
    fs::write(p.join("pkg/calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
    fs::write(
        p.join("tests/test_calc.py"),
        "from pkg.calc import add\n\ndef test_all():\n    assert add(1, 2) == 3\n    assert add(1.5, 2.0) == 3.5\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(p)
        .args(["describe", "add"])
        .env("PYQ_CACHE_DIR", cache.path())
        .output()
        .expect("run pyq");
    let env: Value = match serde_json::from_str(&String::from_utf8_lossy(&out.stdout)) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping: no envelope (pytest unavailable?)");
            return;
        }
    };

    let def = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "definition")
        .expect("a definition row for `add`");

    // `add` was called with ints and floats, so the observed return unions both.
    // Absent only when the suite couldn't run (pre-3.12 / no pytest) — skip then.
    match def["observed_return"].as_str() {
        Some(t) => assert_eq!(t, "float | int", "observed return union; row: {def}"),
        None => eprintln!("skipping assertions: no observed_return (pre-3.12 or no pytest)"),
    }
}

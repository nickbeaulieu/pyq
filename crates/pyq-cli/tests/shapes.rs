//! observed shapes (#9.5) over the built `pyq` binary. Needs python3 + pytest
//! (3.12+ for real shapes); skips otherwise.

use std::fs;
use std::process::Command;

use serde_json::Value;

#[test]
fn shapes_union_observed_return_types() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    fs::create_dir_all(p.join("pkg")).unwrap();
    fs::create_dir_all(p.join("tests")).unwrap();
    fs::write(p.join("pkg/__init__.py"), "").unwrap();
    fs::write(
        p.join("pkg/calc.py"),
        "def add(a, b):\n    return a + b\n\ndef label(n):\n    if n > 0:\n        return \"pos\"\n    return None\n",
    )
    .unwrap();
    fs::write(
        p.join("tests/test_calc.py"),
        "from pkg.calc import add, label\n\ndef test_all():\n    assert add(1, 2) == 3\n    assert add(1.5, 2.0) == 3.5\n    assert label(1) == \"pos\"\n    assert label(-1) is None\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(p)
        .args(["shapes", "-q"])
        .output()
        .expect("run pyq");
    let env: Value = match serde_json::from_str(&String::from_utf8_lossy(&out.stdout)) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping: no envelope (pytest unavailable?)");
            return;
        }
    };

    // Pre-3.12 degrades to empty + warning.
    if env["results"].as_array().map(|a| a.is_empty()).unwrap_or(true) {
        eprintln!(
            "skipping assertions: shapes empty (Python {})",
            env["query"]["python"].as_str().unwrap_or("?")
        );
        return;
    }

    let returns_of = |fqn: &str| -> Vec<String> {
        env["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["owner"].as_str() == Some(fqn))
            .map(|r| {
                r["returns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    // add() saw both int and float; label() saw both str and NoneType.
    assert_eq!(returns_of("pkg.calc.add"), vec!["float", "int"]);
    assert_eq!(returns_of("pkg.calc.label"), vec!["NoneType", "str"]);

    // module-scope frames are filtered out.
    assert!(env["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["owner"].as_str() != Some("pkg.calc")));
}

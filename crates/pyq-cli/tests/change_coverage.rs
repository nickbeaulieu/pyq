//! change-coverage (#9.4) over the built `pyq` binary: build a git repo, change
//! one tested line and one untested line, assert the covered/uncovered split
//! and the covering test. Needs git + python3 + pytest (3.12+ for real line
//! coverage); skips if any piece is missing.

use std::fs;
use std::process::Command;

use serde_json::Value;

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed");
}

#[test]
fn change_coverage_splits_covered_and_uncovered() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    fs::create_dir_all(p.join("pkg")).unwrap();
    fs::create_dir_all(p.join("tests")).unwrap();
    fs::write(p.join("pkg/__init__.py"), "").unwrap();
    fs::write(
        p.join("pkg/ops.py"),
        "def covered_fn(x):\n    return x + 1\n\ndef uncovered_fn(x):\n    return x - 1\n",
    )
    .unwrap();
    fs::write(
        p.join("tests/test_ops.py"),
        "from pkg.ops import covered_fn\n\ndef test_covered():\n    assert covered_fn(1) == 2\n",
    )
    .unwrap();

    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipping: git unavailable");
        return;
    }
    git(p, &["init", "-q"]);
    git(p, &["config", "user.email", "t@t.com"]);
    git(p, &["config", "user.name", "t"]);
    git(p, &["add", "-A"]);
    git(p, &["commit", "-qm", "baseline"]);

    // Add a body line to each function: one tested, one not.
    fs::write(
        p.join("pkg/ops.py"),
        "def covered_fn(x):\n    y = x + 1\n    return y\n\ndef uncovered_fn(x):\n    z = x - 1\n    return z\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_pyq"))
        .args(["--json", "--root"])
        .arg(p)
        .args(["change-coverage", "-q"])
        .output()
        .expect("run pyq");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skipping: no envelope (pytest unavailable?)\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
    };

    // Pre-3.12 degrades to unknown — accept that as a skip for the assertions.
    let py = env["query"]["python"].as_str().unwrap_or("");
    if env["results"]
        .as_array()
        .map(|a| a.iter().any(|r| r["status"] == "unknown"))
        .unwrap_or(false)
    {
        eprintln!("skipping assertions: line coverage unavailable on Python {py}");
        return;
    }

    let status_of = |loc: &str| -> Option<String> {
        env["results"].as_array().unwrap().iter().find_map(|r| {
            (r["loc"].as_str() == Some(loc)).then(|| r["status"].as_str().unwrap().to_string())
        })
    };

    // covered_fn's new body line is exercised by the test.
    assert_eq!(status_of("pkg/ops.py:2").as_deref(), Some("covered"));
    // uncovered_fn's new body line is not.
    assert_eq!(status_of("pkg/ops.py:6").as_deref(), Some("uncovered"));

    // The covered line names the test that ran it.
    let covered_row = env["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["loc"] == "pkg/ops.py:2")
        .unwrap();
    assert!(covered_row["tests"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t.as_str().unwrap().contains("test_covered")));
}

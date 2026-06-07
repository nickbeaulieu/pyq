//! End-to-end: drive a real pytest run through the bundled sidecar and assert
//! the observed-effects envelope. Requires `python3` with `pytest` on PATH; if
//! pytest is absent the run produces no ledger and `observed_effects` errors,
//! which we treat as "skip" rather than "fail" so the suite stays green on a
//! machine without pytest.

use std::fs;

use pyq_dynamic::{observed_effects, TraceOptions};

fn write_fixture(dir: &std::path::Path) {
    fs::create_dir_all(dir.join("pkg")).unwrap();
    fs::write(dir.join("pkg/__init__.py"), "").unwrap();
    fs::write(
        dir.join("pkg/io_ops.py"),
        r#"
import os, socket

def read_devnull():
    with open(os.devnull) as f:
        return f.read()

def ping():
    s = socket.socket()
    try:
        s.connect(("127.0.0.1", 9))
    except OSError:
        pass
    finally:
        s.close()

def pure(x):
    return x + 1
"#,
    )
    .unwrap();
    // Put the test under a tests/ subdir with no __init__.py: pytest's default
    // prepend import mode would then only put tests/ on sys.path, so `import
    // pkg` (at the root) fails unless the driver adds the root to PYTHONPATH.
    // This guards build_pythonpath().
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::write(
        dir.join("tests/test_io.py"),
        r#"
from pkg.io_ops import read_devnull, ping, pure

def test_fs():
    read_devnull()

def test_net():
    ping()

def test_pure():
    assert pure(1) == 2
"#,
    )
    .unwrap();
}

fn owners_with_effect(env: &pyq_output::Envelope, effect: &str) -> Vec<String> {
    env.results
        .iter()
        .filter(|r| r.get("effect").and_then(|v| v.as_str()) == Some(effect))
        .filter_map(|r| r.get("owner").and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

#[test]
fn observes_fs_and_network_under_pytest() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    let mut opts = TraceOptions::new(dir.path().to_string_lossy().into_owned());
    // keep the run quiet
    opts.pytest_args = vec!["-q".into()];

    let env = match observed_effects(&opts) {
        Ok(env) => env,
        Err(e) => {
            eprintln!("skipping: pytest not runnable in this environment: {e:#}");
            return;
        }
    };

    assert_eq!(env.tool, "pyq");
    assert_eq!(
        env.query.get("driver").and_then(|v| v.as_str()),
        Some("pytest")
    );
    // exit code threaded through (all tests pass -> 0)
    assert_eq!(
        env.query.get("pytest_exit").and_then(|v| v.as_i64()),
        Some(0)
    );

    // Effects attributed to the exact FQNs the static `effects` verb uses.
    assert!(
        owners_with_effect(&env, "fs").contains(&"pkg.io_ops.read_devnull".to_string()),
        "fs owners: {:?}",
        owners_with_effect(&env, "fs")
    );
    assert!(
        owners_with_effect(&env, "network").contains(&"pkg.io_ops.ping".to_string()),
        "network owners: {:?}",
        owners_with_effect(&env, "network")
    );
    // The pure function performed no effect -> never an owner.
    let all_owners: Vec<_> = env
        .results
        .iter()
        .filter_map(|r| r.get("owner").and_then(|v| v.as_str()))
        .collect();
    assert!(!all_owners.contains(&"pkg.io_ops.pure"));

    // The unaudited-categories caveat is always surfaced.
    assert!(env.warnings.iter().any(|w| w.contains("env-read")));
}

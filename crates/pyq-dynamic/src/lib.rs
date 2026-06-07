//! The dynamic tier (TASKS.md #9): run the project's code under a bundled
//! Python sidecar and report what it *actually* did at runtime, keyed by the
//! same fully-qualified ids the static index uses — so the two tiers join.
//!
//! Phase 2: the pytest driver. We materialize the sidecar (embedded at build
//! time), run `pytest` under its plugin, and parse the effect ledger the plugin
//! writes. All subprocess/interpreter contact is confined to this crate — the
//! same insulation discipline `ty_backed` applies to ty (see DESIGN.md), so if
//! the way we drive Python changes, the blast radius is one module.
//!
//! Decisions (settled): **pytest-first** (the suite is the repeatable,
//! side-effect-tolerant entrypoint and exactly what effect-diff/change-coverage
//! consume) and **no opt-in flag** (invoking a dynamic verb is consent, the
//! same contract as typing `pytest`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use pyq_output::Envelope;
use serde_json::Value;
use tempfile::TempDir;

/// The sidecar Python package, embedded so the binary is self-contained. Each
/// entry is (relative path under the package dir, file contents).
const SIDECAR_FILES: &[(&str, &str)] = &[
    ("pyq_trace/__init__.py", include_str!("../sidecar/pyq_trace/__init__.py")),
    ("pyq_trace/fqn.py", include_str!("../sidecar/pyq_trace/fqn.py")),
    ("pyq_trace/effects.py", include_str!("../sidecar/pyq_trace/effects.py")),
    ("pyq_trace/ledger.py", include_str!("../sidecar/pyq_trace/ledger.py")),
    ("pyq_trace/__main__.py", include_str!("../sidecar/pyq_trace/__main__.py")),
    ("pyq_trace/pytest_plugin.py", include_str!("../sidecar/pyq_trace/pytest_plugin.py")),
];

/// How to run the dynamic trace.
pub struct TraceOptions {
    /// Project scan root (the same `--root` the static verbs use).
    pub root: String,
    /// The Python interpreter to drive (e.g. `python3`, or a venv's python).
    pub python: String,
    /// Extra arguments passed through to pytest (test paths, `-k`, markers, …).
    pub pytest_args: Vec<String>,
}

impl TraceOptions {
    pub fn new(root: impl Into<String>) -> Self {
        TraceOptions {
            root: root.into(),
            python: default_python(),
            pytest_args: Vec::new(),
        }
    }
}

/// Default interpreter: honour `PYQ_PYTHON`, else `python3`.
pub fn default_python() -> String {
    std::env::var("PYQ_PYTHON").unwrap_or_else(|_| "python3".to_string())
}

/// Run the suite under the effect ledger and return the observed-effects
/// envelope. A non-zero pytest exit (test failures, or no tests collected) is
/// *not* an error: failing tests still execute code, so the ledger is still
/// meaningful. We only fail if the interpreter can't be launched or the plugin
/// never wrote a ledger (a sidecar crash).
pub fn observed_effects(opts: &TraceOptions) -> Result<Envelope> {
    let sidecar = materialize_sidecar().context("materializing the bundled sidecar")?;
    let out_dir = TempDir::new().context("creating the ledger output dir")?;
    let out_path = out_dir.path().join("ledger.json");

    // Capture pytest's own stdout/stderr rather than inheriting it: pyq's
    // stdout must carry *only* the envelope (a `--json` consumer parses it), so
    // pytest's progress chatter is forwarded to our stderr instead.
    let output = Command::new(&opts.python)
        .arg("-m")
        .arg("pytest")
        .arg("-p")
        .arg("pyq_trace.pytest_plugin")
        .args(&opts.pytest_args)
        .arg(&opts.root)
        .env("PYTHONPATH", build_pythonpath(sidecar.path(), &opts.root))
        .env("PYQ_TRACE_ROOT", &opts.root)
        .env("PYQ_TRACE_OUT", &out_path)
        .output()
        .with_context(|| {
            format!(
                "launching `{} -m pytest` (is it installed and is pytest available?)",
                opts.python
            )
        })?;
    eprint!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    let status = output.status;

    if !out_path.exists() {
        anyhow::bail!(
            "the trace sidecar wrote no ledger (pytest exited {}). Is pytest \
             installed in `{}`? Run with the interpreter whose venv has your \
             project + pytest.",
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            opts.python
        );
    }

    let raw = std::fs::read_to_string(&out_path).context("reading the ledger")?;
    parse_envelope(&raw, status.code())
}

/// Build a pyq `Envelope` from the JSON the sidecar wrote, attaching the pytest
/// exit code to the query block so a consumer can tell a clean run from a run
/// whose tests failed (both still produce a ledger).
fn parse_envelope(raw: &str, exit_code: Option<i32>) -> Result<Envelope> {
    let mut v: Value = serde_json::from_str(raw).context("parsing the ledger JSON")?;
    let mut query = v.get_mut("query").map(Value::take).unwrap_or(Value::Null);
    if let (Some(obj), Some(code)) = (query.as_object_mut(), exit_code) {
        obj.insert("pytest_exit".to_string(), Value::from(code));
    }
    let results = match v.get_mut("results").map(Value::take) {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    let summary = v
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let warnings = match v.get_mut("warnings").map(Value::take) {
        Some(Value::Array(a)) => a
            .into_iter()
            .filter_map(|w| w.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    Ok(Envelope::new(query, results)
        .with_summary(summary)
        .with_warnings(warnings))
}

/// Write the embedded sidecar package to a fresh temp dir and return it (kept
/// alive by the caller; dropping it removes the files).
fn materialize_sidecar() -> Result<TempDir> {
    let dir = TempDir::new()?;
    // Group writes by parent dir so we create `pyq_trace/` once.
    let mut dirs: BTreeMap<PathBuf, ()> = BTreeMap::new();
    for (rel, _) in SIDECAR_FILES {
        if let Some(parent) = Path::new(rel).parent() {
            dirs.insert(dir.path().join(parent), ());
        }
    }
    for d in dirs.keys() {
        std::fs::create_dir_all(d)?;
    }
    for (rel, contents) in SIDECAR_FILES {
        std::fs::write(dir.path().join(rel), contents)?;
    }
    Ok(dir)
}

/// `PYTHONPATH` for the run: the sidecar (so `pyq_trace` imports), then the
/// project root (so the target's first-party packages import — pytest's default
/// `prepend` mode only puts the *test file's* dir on the path, not the project
/// root, so a flat-layout `import pkg` would otherwise fail), then any inherited
/// `PYTHONPATH`. All additive — a project with its own layout/conftest still
/// wins because its paths are present too.
fn build_pythonpath(sidecar: &Path, root: &str) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut parts = vec![sidecar.to_string_lossy().into_owned(), root.to_string()];
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        if !existing.is_empty() {
            parts.push(existing);
        }
    }
    parts.join(sep)
}

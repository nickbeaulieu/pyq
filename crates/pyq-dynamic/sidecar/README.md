# pyq dynamic-tier sidecar

The runtime half of pyq (TASKS.md #9). A bundled Python package the pyq CLI runs
*inside the target interpreter* to observe what the code actually does, keyed by
the same fully-qualified ids the static index uses — so the two tiers join.

Static verbs (`effects`/`tests`/`deadcode`) are over-approximate and flagged:
they say "appears to," "no *static* reaching test," "candidate." Their shared
blind spot is dynamic dispatch. This sidecar is the oracle that confirms or
refutes them on exactly those gaps.

## Phase 1 — audit-hook effect ledger (this package)

Installs `sys.addaudithook` (CPython 3.8+) and routes audit events to pyq's
effect taxonomy, attributing each to the **nearest project frame** on the stack
(audit events fire deep in stdlib; the owner is the project function that caused
it). Emits the standard pyq envelope `{tool, query, summary, count, results,
warnings}`.

### Standalone use (development harness)

```bash
python -m pyq_trace --root <project> --script path/to/app.py --out ledger.json
python -m pyq_trace --root <project> --run pkg.module --out ledger.json
```

`--out` keeps the envelope off the target's stdout (the target owns stdout/
stderr). Without it the envelope goes to stdout (quick eyeballing only).

### Driven by pytest (Phase 2 — shipped)

The `pyq-dynamic` Rust crate (the only place pyq touches a subprocess/
interpreter) embeds this package, materializes it to a temp dir, and runs the
suite under the pytest plugin:

```bash
pyq --root <project> trace [pytest args...]            # observed effects (Phase 1/2)
pyq --json --root <project> trace -q                   # machine envelope on stdout
pyq --root <project> effect-diff [pytest args]         # static vs runtime (Phase 3)
pyq --root <project> change-coverage [--base REF] ...  # changed lines × tests (Phase 4)
pyq --root <project> shapes [pytest args]              # observed return types (Phase 5)
```

`change-coverage` (#9.4) joins `git diff --unified=0 <base>` against per-test
line coverage (`sys.monitoring` LINE events, 3.12+): each changed line is
`covered` (naming the pytest nodeids that ran it) or `uncovered`, and changed
files no test reaches are flagged. Pre-3.12 degrades to `unknown` (the audit
effect ledger still works). `shapes` (#9.5) records the concrete return type
each callable produced (`PY_RETURN`, 3.12+), unioned per FQN — runtime evidence
alongside ty's static inference, the first slice of the protocol surface (#21).

`effect-diff` (#9.3) joins the project-wide static effect surface against this
ledger on `(owner FQN, category)`:

- **confirmed** — static predicted it, runtime did it.
- **dynamic-only** — runtime did it, the syntactic static surface couldn't match
  it (e.g. an effect behind a `getattr`-built callee). The reason to run this.
- **static-only** — static predicted it, runtime didn't: over-approximation or a
  path the suite never exercised (change-coverage, #9.4, separates the two).
- **unverifiable** — a category the audit hook can't see (env-read/random/clock/
  global); reported, never treated as over-approximation.

`pyq_trace/pytest_plugin.py` installs the ledger in `pytest_configure` (before
collection, so target import-time effects are captured) and writes the envelope
to `PYQ_TRACE_OUT`. The crate forwards pytest's own stdout/stderr to *its*
stderr so pyq's stdout carries only the envelope, and puts both the sidecar and
the project root on `PYTHONPATH` (pytest's default prepend mode otherwise leaves
a flat-layout `import pkg` unresolvable when tests live in `tests/`). A non-zero
pytest exit (failures, no tests) is not an error — failing tests still execute
code — and the exit code is threaded into `query.pytest_exit`.

## The FQN join (`fqn.py`)

Runtime key = `module_components(relpath_to_root) + co_qualname`, reproducing
`scope_fqn` in `crates/pyq-resolve/src/graph.rs`. Two normalizations the Phase-0
spike pinned down:

- **Synthetic scopes stripped.** `co_qualname` injects `<locals>`/`<module>`/
  `<listcomp>`/`<lambda>`; pyq's scope path has none. They collapse to the
  nearest real named scope (`outer.<locals>.inner` → `outer.inner`).
- **Constructor folding** (`class_node_of`). Instantiation runs `Cls.__init__`,
  but the static call-graph node for a constructor edge is the class `Cls`.
  Consumers joining against call-graph nodes fold `X.__init__` → `X`; the ledger
  records the faithful `X.__init__`.

## What the audit hook can and cannot see (`effects.py`)

| Category | Audited? | How |
|----------|----------|-----|
| fs | ✅ | `open`, `os.mkdir/remove/rename/…`, `shutil.*` |
| network | ✅ | `socket.*`, `ssl.*`, `urllib.Request` |
| subprocess | ✅ | `subprocess.Popen`, `os.system`, `os.exec*`, `os.fork` |
| db | ✅ (sqlite) | `sqlite3.connect`; other DBs ride `network` |
| env (writes) | ✅ | `os.putenv`, `os.unsetenv` |
| import | ✅ | `import` |
| **env (reads)** | ❌ | `os.getenv`/`environ[...]` are plain dict reads, never audited |
| **random** | ❌ | no audit event |
| **clock** | ❌ | no audit event |
| **global** | ❌ | not an audit concept |

The unaudited categories are deferred to the `sys.monitoring` call-target seam
(Phase 4+). Until then the **static** `effects`/`inputs` verbs remain the oracle
for env-reads/random/clock — the ledger emits a warning so "no env effect" is
never misread as "reads no env."

### Import-machinery noise is filtered

Bytecode loading is a cluster of fs events on the interpreter's own files —
reading source, checking/reading `.pyc`, and the atomic write (`os.mkdir
__pycache__`, a temp-file `open`, `os.rename`, plus an `open(fd)` wrap of the
temp descriptor). All of it is suppressed (`ledger._is_loader_fs_noise`): any fs
event whose path is a source/`__pycache__` file, or an `open` on an integer fd
(the descriptor's creation is audited separately). A genuine import-time
`open("config.ini")` touches none of these and survives.

## Tests

```bash
cd crates/pyq-dynamic/sidecar && python -m pytest -q
```

Covers the FQN join + normalizations (`test_fqn.py`), the event→category map
(`test_effects.py`), and end-to-end traces incl. loader-noise suppression and
stdout isolation (`test_end_to_end.py`).

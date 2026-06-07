# pyq — tasks

Open work in priority order, `P1` (highest) → `P5` (lowest). `→ blocked by #N`
marks a dependency. Design rationale lives in DESIGN.md.

## P1 — accuracy + cache (the current direction)
Every verb returns the most accurate answer currently knowable, automatically
(no flag), and repeat runs are dirt cheap. See DESIGN.md — "The accuracy thesis"
and "The analysis cache."
- **#38.4 · Ledger pre-warm + progress streaming.** Have `pyq index` pre-warm the
  runtime ledger (effects/coverage/shapes), not just the parse + graph layers, and
  stream progress during the cold build.
- **#38.5 · Incremental cache (v2).** *Graph layer done:* a graph-cache miss is
  now repaired incrementally — `cache::call_graph` diffs per-file `FileIndex`
  hashes to find changed files, expands to their import component (+ a
  value-reference safety net for deletions), and `CallGraph::record_incremental`
  re-records only those files, reusing the rest. Byte-identical to a from-scratch
  build (edit/add/delete/edge-change all tested); ~10× faster than a full rebuild
  when the edit is localized. *Remaining:* the ledger half — re-run only the tests
  the coverage map ties to changed lines, instead of the whole suite.
- **#39 · Per-row `confidence`, generalized.** `effects` rows already carry
  `confirmed`/`predicted`/`observed`/`unverifiable`; generalize across the other
  folded verbs, and tag the already-exact verbs `proven`.

## P2 — highest leverage
- **#13b · `tests` verb — fixture graph.** pytest fixtures (`@pytest.fixture`
  defs), scopes, fixture→fixture deps, and conftest resolution (fixtures visible to
  sibling/descendant test dirs). Needs new decorator extraction in `pyq-index`.

## P3 — deeper projections / differential
- **#4 · `--baseline` differential in `pyq-output`.** Capture a baseline result
  set; on re-run show added/removed ("did my edit add dead code / new effects").
  The question an iterating agent actually asks.
- **#14 · `blast` verb — symbol-level blast radius.** Transitive reverse-dep
  closure: everything that must change / be re-tested if a symbol's signature
  changes (reverse call graph + import graph + reaching tests).

## P4 — resolution surface & convenience
- **#16 · resolution-surface verbs — `resolve` / re-exports / `imports-from`.**
  Resolve a use site to its fully-qualified symbol; the re-export / `__all__` map
  (canonical import path); resolve a bare local name to its import/def. (Qualified-
  symbol scoping `A.proc` already shipped; this is the broader surface.)
- **#17 · `decorators` verb + framework maps.** Decorator index (`@app.route`,
  `@pytest.fixture`, `@celery.task`, …), specialized into route maps
  (Flask/FastAPI/Django), ORM model maps (SQLAlchemy/Django), and registry/DI maps
  (click/celery/signals/`entry_points`).
- **#19 · `raises` verb — static exception surface.** What a function transitively
  `raise`s and where it's caught. "What can blow up if I call this."

## P5 — polish / big & separate
- **#21 · Spike: convention extraction + protocol/concurrency surfaces.**
  Convention extraction (naming, import style, error-handling/logging idioms —
  scope tightly); protocol/structural conformance (what satisfies `Protocol P`);
  concurrency surface (async/await reachability, threading, locks).

## Enhancement
- **#36 · Broaden native source-root detection.** ty already reads
  `[tool.pytest.ini_options] pythonpath` as extra-paths; broaden to `[tool.ty]
  extra-paths`/`src.root`, `src/`-layout auto-detect, or a `--src-root` flag, so
  bare first-party imports resolve with full ty precision (not just the
  locate-then-resolve sweep's coverage).

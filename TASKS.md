# pyq — tasks

Priority `P1` (highest) → `P5` (lowest). Open work is listed in priority order;
completed work is logged at the bottom. `→ blocked by #N` marks a dependency.

---

## Open

### P2 — highest leverage
- **#12 · `hierarchy` verb — class tree + override map.** Subclasses / supers /
  MRO, abstract methods left unimplemented, and for a base method every override
  (and vice-versa). The high-frequency OO-refactor footgun.
- **#13 · `tests` verb — test↔code map + fixture graph.** Which tests statically
  reach which symbols; pytest fixtures, scopes, deps, conftest resolution.
  Foundation for static change-coverage. (now unblocked: `CallGraph` reverse closure)

### P3 — deeper projections / differential
- **#3 · `deadcode` verb — graph reachability.** Functions/classes reachable from
  no entrypoint or test, over-approximate and flagged (dynamic dispatch, `__all__`,
  framework hooks). (now unblocked: `CallGraph` reachability from entrypoints)
- **#4 · `--baseline` differential in `pyq-output`.** Capture a baseline result
  set; on re-run show added/removed ("did my edit add dead code / new effects").
  The question an iterating agent actually asks.
- **#14 · `blast` verb — symbol-level blast radius.** Transitive reverse-dep
  closure: everything that must change / be re-tested if a symbol's signature
  changes (reverse call graph + import graph + reaching tests). (now unblocked:
  `CallGraph` reverse closure + the import graph)
- **#15 · `card` verb — symbol signature + neighborhood.** One compact context
  pack: signature, decorators, docstring line, def line-range, immediate callers
  + callees + reaching tests. The token-frugal "tell me about X". (now unblocked:
  `CallGraph` depth-1 neighbours)

### P4 — resolution surface & convenience
- **#16 · resolution-surface verbs — `resolve` / re-exports / `imports-from`.**
  `resolve` a use site to its fully-qualified symbol; the re-export / `__all__`
  map (canonical import path); resolve a bare local name to its import/def.
  *Note: qualified-symbol scoping (`A.proc`) already shipped; this is the broader
  resolution surface.*
- **#17 · `decorators` verb + framework maps.** Decorator index (`@app.route`,
  `@pytest.fixture`, `@celery.task`, …), specialized into route maps
  (Flask/FastAPI/Django), ORM model maps (SQLAlchemy/Django), and registry/DI maps
  (click/celery/signals/`entry_points`).
- **#18 · `mock-targets` verb.** Every `mock.patch("a.b.c")` string resolved
  against the graph to flag drifted/invalid patch paths. → blocked by #16
- **#19 · `raises` verb — static exception surface.** What a function transitively
  `raise`s and where it's caught. "What can blow up if I call this." (now
  unblocked: `CallGraph` forward closure)
- **#20 · `canonical` verb — blessed-helper frequency + untested public.** Rank
  internal symbols by use to surface the repo's blessed helpers; plus public
  surface ∩ no reaching test; plus a test inventory with markers.

### P5 — polish / big & separate
- **#9 · Spike: dynamic tier sidecar.** Bundled Python sidecar driven by the Rust
  CLI, using `sys.addaudithook` (effect ledger), `sys.monitoring` (coverage +
  observed shapes), import hooks. Feeds change-coverage + effect-diff. Large,
  separate; static first.
- **#21 · Spike: convention extraction + protocol/concurrency surfaces.**
  Convention extraction (naming, import style, error-handling/logging idioms —
  scope tightly); protocol/structural conformance (what satisfies `Protocol P`);
  concurrency surface (async/await reachability, threading, locks).

### Enhancement (symptom already fixed)
- **#36 · Honor a source root natively for ty precision.** *Shipped:* ty reads
  `[tool.pytest.ini_options] pythonpath` as extra-paths. Residual: broaden to
  `[tool.ty] extra-paths`/`src.root`, `src/`-layout auto-detect, or a `--src-root`
  flag, so bare first-party imports resolve with full ty precision (not just the
  locate-then-resolve sweep's coverage).

---

## Completed

### Verbs & infrastructure
- **#11 · `effects` verb — static effect surface.** Transitive effect surface as
  a projection of `CallGraph`'s forward closure: `fs`/`network`/`subprocess`/
  `env`/`db`/`random`/`clock`/`global` per reachable callable, each attributed to
  the FQN that performs it; plus import-time effects of in-play modules. Syntactic
  call-site matching (suffix-based, alias-following) in `pyq-index`; over-approximate
  and flagged (dynamic/attribute dispatch not followed, so "pure" = "no effect found").
- **#10 · Resolved call/reference graph primitive (`graph` verb).** Transitive
  cross-file call graph keyed by stable fully-qualified node IDs (`pkg.models.User.__init__`),
  durable across edits. Forward (callees) + reverse (callers) breadth-first
  closure, `--depth` cap, cycle-safe; `query.roots` echoes the resolved FQN
  handle, each node carries `depth`/`via`. `CallGraph` in `pyq-resolve` rides the
  locate-then-resolve seam (syntactic FQN + offset, ty call hierarchy for edges).
  The foundation #11/#13/#3/#14/#15/#19 project from.
- **#1 · `inputs` verb** — env / files / CLI args / pydantic settings (DESIGN #2).
- **#2 · `imports`/`deps` verb** — import graph: forward/reverse deps + cycles (DESIGN #3).
- **#5 · CLI integration tests** over `examples/sample`.
- **#6 · Fix stale dispatch comment + add README.**
- **#7 · not-found / empty-symbol UX** — blank symbol is a usage error; dotted names resolve.
- **#8 · Column-convention parity** — verified true Unicode codepoints (confirmed working).
- **#37 · Fully hide ty/syntactic** — one locate-then-resolve API per verb; no `--syntactic`/`engine`/`source`. Includes qualified-symbol scoping (`A.proc`).
- **#24 · Unify `defs` contract** — one answer tagged `role`/`resolves_to` (subsumed by #37).

### Bugs (found exercising the release binary on real repos)
- **#22 · ty path inherits the CLI tree-walk discipline** — `--root` scoping, `.gitignore`/hidden filtering, uniform path anchoring, cwd-independence.
- **#23 · `inputs` env detection** — `setdefault`, `"K" in os.environ`, and `from os import environ`/`import os as o` aliases.
- **#25 · Structured `warnings` array + resolved root** echoed in the envelope.
- **#26 · Parse-error recovery** — walk the recovered AST so a half-edited file still answers.
- **#27 · Attribute-access call sites** — covered by the unified default (ty); the bare-`Name` syntactic blind spot no longer surfaces a silent 0.
- **#28 · `imports --cycles`** — exclude `TYPE_CHECKING`/deferred edges; ordered `a → b → a` paths.
- **#29 · `imports <module>`** — distinguish "not found" (typo) from "found, no edges".
- **#30 · multi-alias CLI option** — record the canonical `--long` form.
- **#31 · uniform envelope `query` block** — `kind`/`target` everywhere (no engine leak).
- **#32 · `refs` folds in call sites** — `callers ⊆ refs`, alias-aware.
- **#33 · same-named defs** — each result tagged `resolves_to` its def; qualified targeting via #37.
- **#34 · function-local variables** — resolved precisely by anchoring ty at the local's offset.
- **#35 · `imports` forward/reverse module identity** — canonicalize to the file-derived id so both compose on source-rooted repos.

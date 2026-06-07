# pyq — tasks

Priority `P1` (highest) → `P5` (lowest). Open work is listed in priority order;
completed work is logged at the bottom. `→ blocked by #N` marks a dependency.

---

## Open

### P2 — highest leverage
- **#12 · `hierarchy` verb — class tree + override map.** Subclasses / supers /
  MRO, abstract methods left unimplemented, and for a base method every override
  (and vice-versa). The high-frequency OO-refactor footgun.
- **#13b · `tests` verb — fixture graph.** The second half of #13: pytest
  fixtures (`@pytest.fixture` defs), scopes, fixture→fixture deps, and conftest
  resolution (fixtures visible to sibling/descendant test dirs). Needs new
  decorator extraction in `pyq-index`. (test↔code map shipped as #13a)

### P3 — deeper projections / differential
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
- **#19 · `raises` verb — static exception surface.** What a function transitively
  `raise`s and where it's caught. "What can blow up if I call this." (now
  unblocked: `CallGraph` forward closure)
- **#20 · `canonical` verb — blessed-helper frequency + untested public.** Rank
  internal symbols by use to surface the repo's blessed helpers; plus public
  surface ∩ no reaching test; plus a test inventory with markers.

### P5 — polish / big & separate
- **#9 · Dynamic tier sidecar.** Bundled Python sidecar driven by the Rust CLI:
  `sys.addaudithook` (effect ledger), `sys.monitoring` (coverage + observed
  shapes), import hooks. The runtime *oracle* that confirms/refutes the
  over-approximate static verbs (`effects`/`tests`/`deadcode`) on their shared
  blind spot — dynamic dispatch. Headline payoffs: **effect-diff** and
  **change-coverage**. Settled: **pytest-first** drive (run the suite under the
  hooks; arbitrary entrypoints later), **no opt-in flag** (invoking a dynamic
  verb is consent, same as `pytest`). Phased:
  - **#9.0 · Phase 0 — de-risk spike.** *Done — GO.* Proved (a) runtime
    frame→static FQN join (`module_components(relpath)+co_qualname`, matching
    `scope_fqn`; normalize `.<locals>.` away, credit observed `X.__init__` to
    static class node `X`); (b) `sys.monitoring` coexists with coverage.py even
    on `COVERAGE_CORE=sysmon` (coverage=id1, pyq=id3; re-runs need
    `restart_events()`); (c) audit hook maps open→fs / socket.*→network with
    correct project-frame attribution, negligible overhead via first-touch
    `DISABLE`. Demonstrated the value prop: a getattr-only `greet` edge the
    static graph can't see was observed.
  - **#9.1 · Phase 1 — audit-hook effect ledger sidecar.** Standalone Python
    package: `sys.addaudithook` → pyq's effect taxonomy, attributed to the
    nearest project FQN. Pre-3.12 compatible. *Known gap:* audit covers
    fs/network/subprocess/db(sqlite)/env-writes/import; env-reads/random/clock/
    global are unaudited → flagged, deferred to the `sys.monitoring` seam.
  - **#9.2 · Phase 2 — `pyq-dynamic` crate + pytest driver.** *Done.* Crate
    embeds the sidecar (materialize-to-tempdir), runs `pytest -p
    pyq_trace.pytest_plugin` via the resolved interpreter (`--python`/
    `$PYQ_PYTHON`, default `python3`), collects the ledger, renders the standard
    envelope. All subprocess contact confined here (mirrors `ty_backed`). New
    `pyq trace [pytest args]` verb (no opt-in flag). Pytest stdout/stderr
    forwarded to pyq's stderr so `--json` stdout stays pure; sidecar + project
    root added to `PYTHONPATH` (prepend-mode flat-layout imports); pytest exit
    threaded into `query.pytest_exit` (non-zero ≠ error — failing tests still
    run code).
  - **#9.3 · Phase 3 — effect-diff.** *Done.* `pyq effect-diff [pytest args]`
    joins the project-wide static effect surface against the observed ledger on
    `(owner FQN, category)` → `confirmed` / `dynamic-only` (runtime hit an
    effect the syntactic surface can't match, e.g. a `getattr`-built callee —
    the payoff) / `static-only` (predicted, unexercised or over-approx) /
    `unverifiable` (category the audit hook can't see: env-read/random/clock/
    global). Dynamic `import` excluded (not a static category). Carries the
    ledger's caveats through. Rides `--baseline` (#4) when landed.
  - **#9.4 · Phase 4 — change-coverage (`sys.monitoring`, 3.12+).** *Done.*
    `pyq change-coverage [--base <ref>] [pytest args]`: parses `git diff
    --unified=0` for changed new-file lines (relativized to the scan root),
    runs the suite under a per-test LINE-event coverage tracker, joins → each
    changed line `covered` (+ the pytest nodeids that ran it) / `uncovered`,
    plus changed files no test reaches. The oracle behind the `tests` caveat.
    Pre-3.12 degrades to `unknown` with a warning (audit-hook effects still
    work). Coverage tracker uses tool id 2, never `DISABLE`s (needs every line),
    caches relpath on the hot path.
  - **#9.5 · Phase 5 — observed shapes (+ import hooks deferred).** *Done
    (shapes slice).* `pyq shapes [pytest args]`: records the concrete return
    type each callable produced at runtime via `PY_RETURN` (3.12+, tool id 4),
    unioned per FQN (`add -> float | int`) — runtime evidence next to ty's
    static inference, the first slice of the protocol surface (#21).
    Module-scope `<module>` returns filtered. Arg-type capture and the
    import-hook import-graph (audit `import` events already land in the effect
    ledger) deliberately left for later — return types are the high-signal half.
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
- **#3 · `deadcode` verb — graph reachability.** Callables reachable from no
  entrypoint, via `CallGraph` forward reachability from a generous root set
  (tests, dunders, decorated hooks, `__all__`, module-scope refs resolved through
  ty, entrypoint files `manage.py`/`wsgi.py`/`urls.py`/`migrations/`/`management/
  commands/`, framework base subclasses `BaseCommand`/`*View`/`*Serializer`/…
  kept whole incl. methods + inner `Meta`, `[project.scripts]`). Over-approximate
  liveness, under-reports death; residual dynamic dispatch flagged (dotted-string
  config paths, callbacks-as-values, getattr, entry-point systems). New index
  fields: `Def.decorated`, `Ref.module_scope`, `FileIndex.dunder_all`. *Tuned on
  real Django repos:* first pass flagged 982 in scoring (test classes + framework
  classes + inner `Meta` as false dead) → 261 after seeding entry-class subtrees
  and expanding framework bases; alice 5.3%, mroi 1.6%. Verified it finds real
  dead code (`toggle_number`) and the residual FPs are string-config (`EXCEPTION_HANDLER`).
- **#13a · `tests` verb — test↔code map.** Which collected tests statically
  reach a symbol, as a projection of `CallGraph`'s reverse closure filtered to
  test nodes (`test_*` functions in `test_*.py`/`*_test.py`, `test_*` methods on
  a collected class: `Test*`-named **or** `*TestCase`-subclassing — unittest/
  Django/DRF, collected by inheritance). Each reaching test carries the `via`
  tree edge and `depth`. Distinguishes "exists but no static test reaches it" (0
  results) from "no such symbol" (empty roots). Framed as a *call-reachability
  lens, not a coverage metric* — for "which tests to run before this edit," not
  "what's my coverage": dynamic dispatch (attribute calls, framework routing,
  signals/Celery) is invisible, so a 0 ≠ untested (`coverage.py` is the oracle
  there), and aggregating into a percentage misleads. Both that and the over-
  approximation are flagged in the warning + README. Fixture graph deferred to
  #13b. *Found exercising it on a real Django repo:* `TestCase`-subclass test
  classes (non-`Test*` names) were missed by a name-only rule — fixed to detect
  `*TestCase` bases; root must be the package root or `pkg.sub` imports don't
  link (documented).
- **#18 · `mock-targets` verb.** Resolve every `mock.patch("a.b.c")` string
  against the project's module/symbol structure and flag *drifted* paths (the
  patch-where-looked-up gotcha — a silently-no-op patch). Built a focused
  syntactic resolver (modules + top-level bound names incl. import bindings +
  class members) rather than waiting on the full #16 resolution surface.
  High-precision: `drifted` only when the module is first-party and the name is
  provably absent; `external`/`dynamic`/`unverifiable` are reported, not flagged.
  Hardened against false positives found on real repos (Django manager/inherited
  members, builtins, nested source roots). Tier-1 third-party: when the tail
  attribute is on an imported *module*, ty follows the import into typeshed /
  site-packages and verifies it there (`time.sleep` valid, `time.slep` drift) —
  gated so it adds no false positives (moved ~60 patches unverifiable→valid
  across three repos, zero new drifts). Tier-2 (types of values, e.g. boto3
  clients) deliberately left unverifiable.
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

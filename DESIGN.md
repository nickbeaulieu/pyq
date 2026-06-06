# pyq — design notes

A queryable static index for Python codebases. The premise: **expose
code-as-graph as composable JSON an agent queries for ground truth**, instead of
re-deriving it by grepping and guessing. Where `ruff`/`pyright`/`ty` emit
human-facing *diagnostics*, pyq emits *relational facts* (who-calls, what-resolves,
what-this-touches) — the gap a checker structurally leaves open.

## What we are deliberately NOT building
- A linter (ruff owns this).
- A type checker (ty owns this).

The filter for any verb: *"is this already one `ruff check` or `pyright` away?"*
If yes, don't rebuild it — unless we can do it **deeper** (see below).

## What IS worth building
1. **Symbol & reference oracle** (shipped, first slice) — `refs` / `callers` /
   `defs`. The grep-replacement: every use/def of a name as data. Pyright computes
   this but only hands it out one position at a time over LSP; there is no clean
   "all callers of X as JSON" CLI. Highest leverage.
2. **Input / config surface** (`inputs`, shipped) — env reads (`getenv`,
   `environ[...]`, `environ.get`, `environ.setdefault`, and `"K" in environ`
   membership tests), literal `open()` paths, CLI args (argparse `add_argument`,
   click `@option`/`@argument`), and pydantic `BaseSettings` fields. Env matching
   is suffix-based so it follows the `import os as o` / `from os import environ`
   aliases; computed keys/paths (and whole-dict `env = os.environ` binds) bucket
   to `<dynamic>` rather than guess. Over-approximate by design. Pure AST walk on
   the syntactic path (no db). "What does this need to run."
3. **Import / dependency graph as data** (`imports`, shipped) — forward deps,
   reverse deps (who imports X, the blast-radius question), and cycles. Syntactic
   today: files map to dotted modules, relative imports resolve against the
   importer's package, and `from pkg import sub` becomes a precise `pkg.sub` edge
   when that submodule exists. Cycles are the non-trivial SCCs over *import-time*
   edges only — `TYPE_CHECKING`-guarded and deferred/function-local imports (the
   patterns that *break* runtime cycles) are excluded — and each is reported as
   an ordered `a → b → … → a` path. Will ride the resolved graph from #1 later.
4. **Call/reference graph** (`graph`, shipped) — the transitive, cross-file call
   graph keyed by stable fully-qualified node ids (durable across edits).
   Forward (callees) and reverse (callers) closure, `--depth` capped, cycle-safe.
   The foundation the projections below are built on; rides ty's call hierarchy
   anchored on the syntactic index's offsets (see Architecture).
5. **Effect surface** (`effects`, shipped) — the first projection of `graph`:
   does a symbol, or anything it transitively calls, touch the filesystem,
   network, a subprocess, the environment, a database, randomness, the clock, or
   module-global state — plus import-time effects of the modules involved.
   Syntactic and over-approximate: each call site is matched on its dotted callee
   (suffix-based, alias-following, like `inputs`), so a hit means "appears to,"
   and effects behind dynamically/attribute-dispatched calls are not followed.
   "Is this pure / safe in a test / will it hit the network."
6. **Mock-target drift** (`mock-targets`, shipped) — resolve every
   `mock.patch("a.b.c")` string against the project and flag paths that no
   longer exist. The point ruff/pyright miss: `patch` binds *where a name is
   looked up*, not where it's defined, so the index recording import bindings as
   defs is exactly what makes this resolvable. High-precision by construction —
   `drifted` only when the prefix is a project module and the name is provably
   absent; third-party (`external`), computed (`dynamic`), non-class-attribute,
   builtins reached via the module namespace, and missing members on a class
   that extends a base (possibly inherited / framework-injected) are reported
   but never flagged. The last two were false positives found running it against
   a real Django repo — the index records class bases so they can be suppressed.
   When the tail attribute is on an imported *module*, ty follows the import into
   typeshed / site-packages and verifies it there (so `time.sleep` is valid and
   `time.slep` is real drift) — the one place `mock-targets` reaches into
   third-party code, gated to genuine module bindings and `__getattr__`-free
   modules so it adds no false positives.

7. **Dead code** (`deadcode`, shipped) — callables reachable from no entrypoint,
   via forward reachability over the call graph (the deeper-than-"unused import"
   version below). The hard part is the root set: Python has no single `main`,
   and most live code is entered by convention/config, so the bias is toward
   calling things live (a flagged live handler is the dangerous failure). Roots:
   tests, dunders, decorated hooks, `__all__`, module-scope references (resolved
   through ty), entrypoint *files* (`manage.py`/`wsgi.py`/`urls.py`/`migrations/`/
   `management/commands/`/…), framework base subclasses (`BaseCommand`/`*View`/
   `*Serializer`/`*Form`/`*Model`/… — class + methods + inner `Meta` kept whole),
   and `[project.scripts]`. Over-approximate liveness, under-reports death; the
   residual false positives are genuinely dynamic (dotted-string config paths,
   callbacks-as-values, `getattr`, entry-point systems) and flagged.

## Worth building *deeper* than the existing tools (the exception to the filter)
These exist elsewhere but a pyq-native version is better because it rides the
index we already build:
- **Dead code at a deeper level** — not "unused import" but "function reachable
  from no entrypoint / test", across the resolved graph (over-approximate,
  flagged). *Shipped as `deadcode` (#7 above).*
- **Change-coverage** — given a diff, which changed lines are exercised by which
  tests; which changed files have zero test reachability. (Dynamic half later.)

## Architecture
- `pyq-resolve` — the `Resolver` trait and its **one shipping impl**,
  `UnifiedResolver`. No user-visible engine, no fork — *locate-then-resolve*: the
  syntactic index from `pyq-index` locates every place a name is bound or used
  (function-locals, params, `import` bindings — all the offsets a name-level
  symbol table misses), and ty (`ty_ide` + `ty_project`) resolves each precise
  offset semantically (real binding through imports, re-exports, aliasing,
  scope-aware). A sweep anchors ty once per distinct binding (covered-set, so a
  binding costs one ty call however often it appears), so every result is exact
  and same-named bindings resolve separately — each tagged with the def it
  resolves to. No over-approximate tier, nothing to disclose. Results carry a
  `role`; bindings/ambiguous uses carry `resolves_to`.
  - **The transitive call graph (`CallGraph`)** rides the same seam: the
    syntactic index assigns each callable a stable fully-qualified id (module
    path + enclosing scopes + name) and records its name offset; ty's call
    hierarchy supplies the edges, anchored at that same offset, so a neighbour
    maps straight back to its FQN and the walk recurses by re-feeding the offset.
    A breadth-first closure (forward = callees, reverse = callers) is the
    foundation primitive the heavier verbs — blast radius, dead code, the symbol
    card — are projections of. Nodes are durable ids, not line numbers, so an
    agent holds them across edits.
  - **All ty contact is confined to `ty_backed`.** ty is `0.0.x`, so this
    insulation is load-bearing: pin to a ruff tag (churn becomes a scheduled
    upgrade, not runtime flake), depend only on the LSP-shaped entry points
    (`find_references`/`goto_definition`/`all_symbols`/`call_hierarchy`/`rename`
    — the most stable layer), and if its API moves the blast radius is one
    module. Costs accepted: larger binary + vendored typeshed, a Salsa db
    lifecycle, occasional tag-bump migrations.
  - *Why ty over `ruff_python_semantic`:* the latter is externally-driven and
    single-module — using it means reimplementing ruff's `Checker` traversal and
    hand-building all cross-file linking. ty ships it correct and project-wide.
- `pyq-index` — one parse per file (`ruff_python_parser`) → `FileIndex` of defs +
  refs (each with a byte offset). Parse errors non-fatal (an agent mid-edit still
  gets answers). Backs the type-free verbs (the `inputs`/config surface) that
  need no db, and is the *locator* half of `UnifiedResolver`.
- `pyq-output` — the one envelope `{ tool, query, summary, count, results,
  warnings }` with human (default, even piped) + `--json`/`--pretty` renderers.
  `warnings` surfaces what a query couldn't do precisely (e.g. an `inputs`
  `<dynamic>` bucket) so a consumer knows when to fall back to reading the file.
  Will grow the `--baseline` differential ("did my edit add dead code / new
  effects") generic over result sets — the question an iterating agent asks.
- `pyq-cli` — clap, verb-per-invocation, `ignore`-based tree walk (respects
  `.gitignore`). Routes `refs`/`callers`/`defs` through `UnifiedResolver`
  (`callers` via ty's `call_hierarchy`, labelling each call site with its
  enclosing function); `inputs`/`imports` are pure syntactic facts.

## Still open
- **Dynamic tier.** Python's clean seams are `sys.addaudithook` (effect ledger,
  free in CPython), `sys.monitoring` (3.12+, coverage + observed-shape), and
  import hooks. Would ship as a bundled Python sidecar the Rust CLI drives.
  Separate, larger commitment; static first.

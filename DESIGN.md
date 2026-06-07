# pyq — design notes

A queryable static index for Python codebases. The premise: **expose
code-as-graph as composable JSON an agent queries for ground truth**, instead of
re-deriving it by grepping and guessing. Where `ruff`/`pyright`/`ty` emit
human-facing *diagnostics*, pyq emits *relational facts* (who-calls,
what-resolves, what-this-touches) — the gap a checker structurally leaves open.

## What we are deliberately NOT building
- A linter (ruff owns this).
- A type checker (ty owns this).

The filter for any verb: *"is this already one `ruff check` or `pyright` away?"*
If yes, don't rebuild it — unless we can do it **deeper**: reachable-from-no-
entrypoint rather than "unused import"; the effect surface across the resolved
call graph; the runtime-verified version of a static answer.

## Architecture
- `pyq-resolve` — the `Resolver` trait and its one shipping impl,
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
    foundation primitive the heavier verbs are projections of. Nodes are durable
    ids, not line numbers, so an agent holds them across edits.
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
  gets answers). Backs the type-free verbs (the `inputs`/config surface) that need
  no db, and is the *locator* half of `UnifiedResolver`.
- `pyq-output` — the one envelope `{ tool, query, summary, count, results,
  warnings }` with human (default, even piped) + `--json`/`--pretty` renderers.
  `warnings` surfaces what a query couldn't do precisely so a consumer knows when
  to fall back to reading the file.
- `pyq-cli` — clap, verb-per-invocation, `ignore`-based tree walk (respects
  `.gitignore`). Routes `refs`/`callers`/`defs` through `UnifiedResolver`;
  `inputs`/`imports` are pure syntactic facts.
- `pyq-dynamic` — the bundled Python sidecar (materialized to a tempdir) driven by
  the Rust CLI over `sys.addaudithook` (effect ledger) and `sys.monitoring`
  (coverage + observed return shapes), pytest-first. All subprocess/interpreter
  contact is confined here, mirroring `ty_backed`. The runtime frame → static
  `scope_fqn` join is exact, so its ledger keys join the static graph by FQN.

## The accuracy thesis — automatic verification
Every verb returns *the most accurate answer currently knowable*, with no flag and
no unlabeled guess. Two truths force the shape:
- Static analysis of Python can't be exact *and* sound at once (Rice's theorem,
  made vicious by `getattr`/`eval`/`importlib`/metaclasses/monkeypatching) — it
  must over- or under-approximate.
- Dynamic analysis is exact only for the paths that actually ran — it
  under-approximates to whatever the suite exercised.

So there is no single oracle that yields one true answer, and "100% accuracy" is
the wrong target. What we *can* eliminate is **unqualified** approximation: every
result carries a structural `confidence` — `proven` (statically decidable, exact),
`observed` (ran this suite run), `confirmed` (predicted *and* observed), `predicted`
(static reach, may be a phantom edge or miss a dynamic one), `refuted` (static said
so, runtime contradicted it), `unverifiable` (undecidable for this evidence). The
over-approximate verbs (`effects`/`tests`/`deadcode`/`canonical`/reverse `graph`)
fuse the runtime ledger **automatically** and relabel each row; the already-exact
verbs (`defs`/`refs`/`callers`/forward `graph`/`imports`) are `proven` and never run
the suite. The irreducible residue — code with zero test coverage, statically
undecidable inputs (`<dynamic>` keys) — is *named* `unverifiable`, not hidden: that
label tells the agent exactly which file to open.

Consequence: **the dynamic verbs fold into their static counterparts.** They were
only separate because you had to opt into running the suite; remove that and
`effect-diff` is just what `effects` returns, observed return types fold into
`describe`/`defs` (declared next to observed), `change-coverage` is `tests` seeded
from a diff, and `trace` demotes to an internal ledger dump. Fewer verbs, each more
accurate. Degrade to static `predicted` (never error, never hang) when there's no
interpreter / no pytest / no collected tests / a pre-3.12 runtime.

## The analysis cache — first run pays, every run after is dirt cheap
The first verb on a repo is expensive (walk the tree, parse every file, build the
ty-derived graph); every verb after is near-free, served from a content-addressed
snapshot under `~/.pyq/` — a global, per-repo-namespaced store
(`~/.pyq/cache/<canonical-root-hash>/`, with room for config/logs later). Three
layers share one fingerprint:
- **parse** — per-file `FileIndex` (defs/refs/effects/inputs), keyed by each file's
  content hash; only changed files re-parse.
- **graph** — the ty-*derived* facts (resolved call edges forward+reverse, override
  map, hierarchy, `resolves_to`), keyed by the whole-tree fingerprint. We cache the
  derived relations, **not ty's Salsa DB** (0.0.x, not durably serializable) — so
  the cheap path never starts the parser or ty; it operates purely on the
  materialized graph. The cache *is* the code-as-graph this tool exposes.
- **ledger** — runtime effects/coverage/shapes by FQN, keyed by the tree
  fingerprint, from one instrumented suite run (audit hook + `sys.monitoring` tool
  ids coexist).

Validation is the load-bearing part: a `stat` sweep (`size` + `mtime_ns`) reusing
the ignore-walk we already do, with a `blake3` content hash only on files whose stat
moved — a clean repo hashes nothing, so a warm verb is a stat sweep plus an `mmap`
deserialize. Build-on-miss is lazy and automatic; `pyq index` pre-warms the parse +
graph layers explicitly. Writes are lockfile-guarded with atomic temp-then-rename so
a fanned-out agent never races a torn cache. Today a source change rebuilds graph +
ledger wholesale (reusing unchanged parses); v2 will incrementalize — re-resolve only
edges touching changed FQNs, and re-run only the tests the coverage map ties to
changed lines. The bigger-but-faster alternative — a resident daemon holding the
graph in memory (the rust-analyzer model) — buys microsecond repeats at the cost of
IPC + lifecycle + a live ty DB; the cache-backed CLI gets ~95% of the win at ~20% of
the cost and is the right first build.

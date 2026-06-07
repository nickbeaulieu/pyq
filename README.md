# pyq

A queryable static index for Python codebases. pyq exposes **code-as-graph as
composable JSON an agent queries for ground truth** — who-calls, what-resolves,
what-this-touches — instead of re-deriving it by grepping and guessing.

Where `ruff`/`pyright`/`ty` emit human-facing *diagnostics*, pyq emits
*relational facts*. It deliberately does **not** rebuild what a linter or type
checker already gives you (see [DESIGN.md](DESIGN.md)); it fills the gap they
structurally leave open.

## Install

```bash
cargo build --release
# binary at target/release/pyq
```

Requires the Rust toolchain pinned in `rust-toolchain.toml`. The Python
parsing/semantic layer is vendored from a pinned `ruff`/`ty` tag, so the first
build fetches and compiles those crates.

## Usage

One verb per invocation: `pyq <verb> [args] [flags]`.

```bash
pyq refs User                 # every reference to `User` across the tree
pyq callers make_user         # every call site of `make_user`
pyq defs User                 # every definition of `User`
pyq graph main                # everything `main` transitively calls
pyq graph User --reverse      # everything that transitively reaches `User`
pyq effects handle_request    # side effects it transitively performs (io/net/db/…)
pyq tests add                 # which tests are call-wired to `add` (run before editing)
pyq mock-targets              # resolve every mock.patch("…") — flag drifted paths
pyq hierarchy Animal          # supertypes, subclasses, and the override map
pyq deadcode                  # callables reachable from no entrypoint (candidates)
pyq inputs                    # the external input surface of the project
pyq imports pkg.models --reverse   # who imports pkg.models (blast radius)
pyq imports --cycles          # import cycles among project modules
```

### Verbs

| Verb | Answers |
|------|---------|
| `refs <symbol>` | Every reference (reads, writes, calls) to a symbol, cross-file. |
| `callers <symbol>` | Every call site of a symbol. |
| `defs <symbol>` | Every definition (function/class/variable/import binding), each tagged `role` (`definition`/`binding`); a `binding` points at its canonical def via `resolves_to`. |
| `graph <symbol>` | The transitive call graph: everything the symbol calls (forward closure), or — with `--reverse` — everything that calls it. Nodes are stable fully-qualified IDs (`pkg.models.User.__init__`) re-queryable after edits; `--depth N` caps the hops. |
| `effects <symbol>` | The transitive effect surface: which side effects (`fs`, `network`, `subprocess`, `env`, `db`, `random`, `clock`, `global`) the symbol and everything it transitively calls statically perform, plus import-time effects of the modules involved. "Is this pure / safe in a test." |
| `tests <symbol>` | A call-reachability lens (**not** a coverage metric): which collected tests are structurally wired to a symbol via the reverse call graph, each with the call path (`via`) and `depth`. A test is a `test_*` function in `test_*.py`/`*_test.py`, or a `test_*` method on a collected class — `Test*`-named **or** `*TestCase`-subclassing (unittest/Django/DRF). For "which tests to run before this edit," not "what's my coverage." Blind to dynamic dispatch (attribute calls, framework routing, signals) — a 0 is "no *static* reaching test," not "untested." See [Tests](#tests). |
| `mock-targets` | Resolve every `mock.patch("a.b.c")` target against the project and flag *drifted* paths — a patch whose looked-up name no longer exists silently no-ops, so the test passes while exercising the real code. |
| `hierarchy <class>` | The class's supertypes (bases, external marked), transitive subclasses, and the override map — which base methods it overrides and which subclasses override its methods. Resolved across files by ty, subclasses computed by inverting the supertype graph. The OO-refactor footgun, as data. |
| `deadcode` | Functions/classes reachable from **no** entrypoint — candidate dead code, via forward reachability over the call graph. Roots are everything the runtime/framework enters without a project call: tests, dunders, decorated hooks, `__all__`, module-scope calls, entrypoint files (`manage.py`/`wsgi.py`/`urls.py`/`migrations/`/`management/commands/`/…), framework base classes (`BaseCommand`/`*View`/`*Serializer`/…), and `[project.scripts]`. Over-approximate liveness (so it under-reports death); residual dynamic dispatch is flagged. See [Dead code](#dead-code). |
| `inputs` | What the code needs to run: env vars, literal files opened, CLI args (argparse/click), pydantic settings fields. |
| `imports [module]` | The import graph. No arg: every edge. With a module: what it imports; `--reverse`: who imports it (blast radius); `--cycles`: import cycles. Accepts a module name or a file path. |

### Flags

All flags are global (accepted before or after the verb).

| Flag | Effect |
|------|--------|
| `--root <dir>` | Directory to scan. Defaults to the current directory. |
| `--json` | Emit the compact single-line JSON envelope. |
| `--pretty` | Emit indented JSON. |

## One answer per verb

`refs`/`callers`/`defs` run a single engine — there is no flag to choose, and the
output never names one. Under the hood it's *locate-then-resolve*: a one-parse
syntactic index locates every place a name is bound or used — including
function-locals, parameters, and `import` bindings a name-level symbol table
never surfaces — and ty resolves each precise location semantically (real
binding through imports, re-exports, and aliasing, scope-aware). So every result
is exact; there's no over-approximate tier to disclose.

Results carry a `role` (`definition`/`binding`/`reference`/`call`); a `binding`,
and any use of an ambiguous (same-named) symbol, points at its canonical def via
`resolves_to`. `inputs`/`imports` are pure syntactic facts, over-approximate by
design: computed env keys or paths bucket to `<dynamic>` rather than guess.

A qualified query scopes to the named def: `Alpha.process` is that class's method
(its callers exclude `Beta.process`'s), and `pkg.models.User` is the class in
`pkg/models.py`, not another module's `User`. The qualifier matches as a suffix
of the def's scope path (module components + enclosing classes/functions), so
`models.User` works too. A bare name (`process`) still unions all defs, each
tagged with `resolves_to` so you can filter.

## Call graph

`graph` is the transitive call/reference primitive the heavier verbs build on. It
walks ty's resolved call hierarchy from the queried symbol — forward (callees) by
default, `--reverse` for callers — and returns the reachable set as a closure,
deduped and cycle-safe. Each node is a **stable fully-qualified ID**
(`pkg.models.User.__init__`) derived from the module path and enclosing scopes,
not a line number — so an agent can hold a node id across edits and re-query it
without re-grepping. The resolved root FQN(s) are echoed in `query.roots`; every
node carries its `depth` from the root and the `via` (the FQN it was first reached
through), enough to reconstruct a path back. `--depth N` caps the walk.

The reverse closure follows bare-name and imported-name call sites
(`from m import f; f()`). Calls through an attribute (`obj.method()`) and other
dynamic dispatch are not resolved as reverse edges — the same boundary the
`callers` verb has — so a method reached only via `self.x.method()` may show
fewer callers than exist. The forward closure has no such gap (it reads each
def's own body).

```console
$ pyq graph main --root examples/sample
2 nodes reachable from `main`
pkg/models.py:1:7  class pkg.models.User (depth 1, via app.main)
pkg/models.py:5:5  function pkg.models.make_user (depth 1, via app.main)
```

## Effects

`effects` is the first projection of the call graph: it walks the forward
closure of a symbol and, for every reachable callable, scans its body for
side-effecting calls — `open`/`shutil` (`fs`), `requests`/`httpx`/`socket`
(`network`), `subprocess`/`os.system` (`subprocess`), `os.getenv`/`os.environ`
(`env`), `*.execute`/`sqlite3.connect` (`db`), `random`/`secrets`/`uuid`
(`random`), `time`/`datetime.now` (`clock`), and `global` declarations
(`global`). Each hit is attributed to the FQN that actually performs it, so a
"pure-looking" entry point reveals the network call three hops down.

```console
$ pyq effects run --root path/to/project
effects of `run`: db, network, random — 4 sites
io_ops.py:6:12   network requests.get     in io_ops.fetch
io_ops.py:9:12   db      sqlite3.connect   in io_ops.save
io_ops.py:10:5   db      conn.execute      in io_ops.save
io_ops.py:14:12  random  random.random     in io_ops.jitter
! static over-approximation: effects behind dynamic/attribute-dispatched calls are not followed
```

Module- and class-body effects are flagged `import-time` (they run on import,
not on call) and reported for every module that contributes a reachable
callable. Detection is syntactic and over-approximate by design: a hit means the
code *appears* to perform the effect, and — as with `callers` — effects behind
calls that resolve through attribute/dynamic dispatch aren't followed, so "pure"
means "no effect found," not a proof of purity.

## Tests

`tests` is a reverse-call-graph projection: it walks the closure of callers of a
symbol and keeps the ones a test runner would collect — `test_*` functions in
`test_*.py`/`*_test.py`, and `test_*` methods on a collected class (`Test*`-named
**or** subclassing a `*TestCase`: unittest, Django, DRF — collected by
inheritance). Each reaching test carries the `via` edge and `depth`, so you see
the call path, not just the fact.

```console
$ pyq tests make_user --root path/to/project
2 tests reach `make_user`
tests/test_users.py:8:5   tests.test_users.test_make_user reaches `make_user` (depth 1, via app.make_user)
tests/test_api.py:31:9    tests.test_api.UserApiTests.test_signup reaches `make_user` (depth 3, via app.signup)
```

**This is a call-reachability lens, not a coverage metric.** It answers "which
tests are structurally wired to this symbol," exactly, for the call edges it can
resolve — the question worth asking *before* an edit (which tests to run, what
might break) without a test DB or a suite run. It is **not** a substitute for
`coverage.py`, and a percentage built by aggregating it over many symbols will
mislead. Reach for it per-symbol, at edit time, and verify what matters with a
real coverage run.

Two boundaries, both load-bearing — do not read a zero as "untested":

- **Dynamic dispatch is invisible.** Reach through an attribute call
  (`obj.method()`, pydantic's `Model.model_validate(...)`), framework routing
  (Flask/FastAPI/Django/DRF views reached via URL dispatch), signals and Celery
  tasks (`signal.send()`, `.delay()`), registries, or `getattr` is **not**
  followed — the same boundary `callers`/`graph --reverse` have. A view or signal
  handler exercised only through the framework will show **0 reaching tests while
  being fully tested at runtime.** Framework-dispatched code is exactly where you
  should trust `coverage.py` instead.
- **Reachability ≠ execution.** A static caller edge may sit on a branch a given
  test never takes (over-approximation), and collection uses the default
  pytest/unittest naming + inheritance rules — custom `python_files`/
  `python_classes` config is not read.

A symbol that exists but is reached by no test is a `0`-result success (a
candidate gap); a symbol that names no function or class is a distinct
empty-`roots` answer (a typo) — so you can tell "structurally untested" from "no
such symbol." **Scope `--root` to the package root where first-party imports
resolve** (the directory whose children are your top-level packages), or
absolute `pkg.sub`-style imports won't link and reach will be silently
under-reported.

## Mock-target drift

`mock.patch` replaces a name *where it is looked up*, not where it is defined —
so a test patches `myapp.client.requests` because `client.py` does `import
requests`. Refactor that import away and the patch silently does nothing: the
test keeps passing while it now exercises the real `requests`. `mock-targets`
resolves every `patch("…")` string against the project's actual module/symbol
structure (import bindings included, since the index records them) and flags the
ones that no longer resolve.

```console
$ pyq mock-targets --root path/to/project
8 patch targets, 2 drifted
tests_demo.py:13:10  drifted myapp.client.Client.gone — `gone` is not a member of `myapp.client.Client`
tests_demo.py:17:10  drifted myapp.client.deleted_fn — `deleted_fn` is not bound in module `myapp.client`
tests_demo.py:4:2    valid myapp.client.requests
tests_demo.py:11:10  valid myapp.client.Client.fetch
tests_demo.py:19:10  external os.path.exists
tests_demo.py:22:10  dynamic <dynamic>
...
! drifted patch `myapp.client.Client.gone` (tests_demo.py:13:10): `gone` is not a member of `myapp.client.Client`
! drifted patch `myapp.client.deleted_fn` (tests_demo.py:17:10): `deleted_fn` is not bound in module `myapp.client`
```

Precision over recall: a target is `drifted` only when its prefix is a *project*
module and the looked-up name is provably absent. Everything that can't be
proven absent is reported but never flagged broken — targets into third-party /
stdlib modules (`external`), computed non-literal targets (`dynamic`), an
attribute on a non-class binding (`unverifiable`), a builtin reached through the
module namespace (`patch("mod.open")` → `valid`), and a missing member on a
class that extends a base, which may be inherited or framework-injected
(Django's `objects` manager, `Model._save_table` → `unverifiable`). So a flagged
drift is a real one. (`patch.object` / `patch.dict`, whose target isn't a dotted
string, are out of scope.)

Module spellings are matched honoring a source root: on a nested layout (files
at `alice/main/services.py`, imported and patched as `main.services.*`) the
target resolves to the canonical file-derived id by unique suffix — so the verb
doesn't silently degrade to "all external" and check nothing.

When a target's tail attribute is on an imported **module** (`patch("svc.time.sleep")`,
where `svc.py` does `import time`), resolution follows the import into that
module — including typeshed and installed site-packages — and verifies the
attribute there: `time.sleep` is `valid`, a typo like `time.slep` is real
`drifted`. This is gated to genuine module bindings (a `from m import func`
binding is a value, not a module, and stays `unverifiable`), and to modules
without a dynamic `__getattr__`, so it never manufactures a false drift —
verified across three real repos, where it moved ~60 patches from `unverifiable`
to `valid` (e.g. `time.sleep`) and added zero new drifts.

## Hierarchy

`hierarchy <class>` resolves the project's class inheritance graph and answers
the OO-refactor questions as data: the class's **supertypes** (bases, with
external/framework bases flagged), its transitive **subclasses**, and the
**override map** — which base methods it overrides, and which subclasses override
its own methods. ty resolves each class's immediate bases across files and
through imports; subclasses are computed by inverting that graph (ty's own
subtype search is unreliable), so "change this base method — who's affected?" is
one query.

```console
$ pyq hierarchy Animal
7 relations for `Animal`
animals.py:3:7   supertype ABC (external)
animals.py:7:7   subtype animals.Dog
animals.py:10:7  subtype animals.Puppy
animals.py:8:9   animals.Dog.speak overrides animals.Animal.speak
animals.py:11:9  animals.Puppy.speak overrides animals.Animal.speak
```

The same graph powers two other verbs: `deadcode` reads it for override-aware
reachability and the external-base liveness signal (below), and `mock-targets`
uses it to resolve a method inherited from a first-party base.

## Dead code

`deadcode` runs the call graph *forward from the program's entrypoints* and
reports the callables nothing reaches. Python has no single `main`, and most
live code is entered by **convention or config**, not a project call — so the
verb's real work is the root set, and the bias is heavily toward calling things
live (flagging a live route handler dead is the dangerous failure). Roots:

- pytest-collected tests, and every method of a collected test class;
- dunder methods (`__init__`, `__enter__`, …);
- decorated callables (routes, fixtures, tasks, CLI commands, signals);
- `__all__` exports;
- callables referenced at module scope (`__main__`, URL tables, registries),
  resolved through ty;
- everything in an entrypoint *file* (`manage.py`, `wsgi.py`/`asgi.py`,
  `urls.py`, `settings`, `conftest.py`, `migrations/`, `management/commands/`,
  `scripts/`, `setup.py`);
- the whole subtree (methods + inner `Meta`/`Config`) of any class that extends
  an **external base** — one ty can't resolve to a first-party class, *anywhere
  up the chain* — since the framework drives it. This generalizes the old
  curated base-name list via the `hierarchy` graph: a Django command's `handle`,
  a DRF serializer's `get_*`, a `BasePermission`'s `has_permission`, and a model
  `Foo(TimeStampedModel)` (external `models.Model` is a *transitive* base) are
  all live;
- dotted-string config paths and `[project.scripts]` / `[tool.poetry.scripts]`
  console entrypoints.

It is also **override-aware** (the `hierarchy` payoff): a reachable base method
makes its overrides reachable, recovering the polymorphic edge the call graph
misses (a base-typed `x.method()` resolves to the base, not each concrete
override). This is what eliminated the dominant false-positive class — overrides
of framework interface methods (`has_permission`, `parse`, `label_from_instance`)
that looked dead because nothing calls them *directly*.

```console
$ pyq deadcode --root path/to/project
2 dead-code candidates of 14 callables
app/core.py:12:5  function app.core.truly_dead
app/core.py:16:5  function app.core.orphan_helper
! over-approximate — verify before deleting. …reached dynamically (a dotted-string
  path in config, a callable passed as a value, getattr/reflection, an
  entry-point system) is not dead.
```

It's **over-approximate**: a candidate is reported when no *static* call path
reaches it, which is not the same as dead. The residual false positives are
genuinely dynamic and flagged — a dotted-string path in config (Django
`EXCEPTION_HANDLER`/`MIDDLEWARE`, Celery task names), a callable passed as a
value (`side_effect=`, callbacks, registries), `getattr`/reflection, or a
plugin/entry-point system pyq doesn't read. So treat the output as candidates to
verify, not a delete list. (Conversely it under-reports: a class whose every
method the framework drives is kept whole, so a genuinely dead method on a live
serializer won't surface.) On three real repos it lands at 1.6–5.3% of
callables. Run it from the package root so cross-module imports resolve.

## Output envelope

Every verb emits the same shape. The default human view is a token-frugal
summary line plus one result per line (used even when piped); `--json` opts into
the structured envelope:

```json
{
  "tool": "pyq",
  "query": { "kind": "inputs" },
  "summary": "11 inputs",
  "count": 11,
  "results": [
    { "loc": "config.py:3:9", "label": "env DEBUG" },
    { "loc": "cli.py:14:5",   "label": "arg --verbose" }
  ]
}
```

Locations are `path:line:col` (1-based, UTF-8 character columns).

## Example

```console
$ pyq inputs --root examples/sample
11 inputs
config.py:3:9   env DEBUG
config.py:4:10  env DATABASE_URL
config.py:5:11  env TIMEOUT
config.py:7:10  env <dynamic>
config.py:10:10 file settings.ini
cli.py:7:5      setting db_url
cli.py:8:5      setting port
cli.py:14:5     arg --verbose
...
```

## Workspace

| Crate | Role |
|-------|------|
| `pyq-cli` | clap front end; verb-per-invocation; `.gitignore`-respecting tree walk. |
| `pyq-index` | One parse per file → `FileIndex` of defs/refs/inputs/effects. Backs the syntactic path. |
| `pyq-resolve` | Cross-file resolution behind a `Resolver` trait, plus the transitive `CallGraph`, backed by ty. All ty contact lives here. |
| `pyq-output` | The shared envelope and its human / `--json` / `--pretty` renderers. |

See [DESIGN.md](DESIGN.md) for the thesis, the roadmap, and the
ty-vs-`ruff_python_semantic` decision.

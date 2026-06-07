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
curl -fsSL https://raw.githubusercontent.com/nickbeaulieu/pyq/main/install.sh | sh
```

This downloads the right prebuilt binary for your platform (macOS and Linux,
x86_64 and arm64), verifies its checksum, installs it to `~/.local/bin`, and
records the channel under `~/.pyq/`. Override either:

```bash
PYQ_CHANNEL=canary PYQ_INSTALL_DIR=~/bin \
  curl -fsSL https://raw.githubusercontent.com/nickbeaulieu/pyq/main/install.sh | sh
# or, if you've cloned the repo: ./install.sh --canary --dir=~/bin
```

### Channels & upgrading

pyq ships on two channels. **stable** tracks tagged releases; **canary** rolls
forward with every push to `main`. Switch between them and update in place
without re-running the installer:

```bash
pyq channel            # show the current channel and this build's identity
pyq channel canary     # follow canary from here on (records intent only)
pyq upgrade            # pull the latest build on the current channel, in place
pyq upgrade --check    # report what an upgrade would do, without installing
```

`upgrade` verifies the download's sha256 before replacing the running binary,
and `--version` pins the exact build (`channel`, date, commit).

### Build from source

```bash
cargo build --release      # binary at target/release/pyq
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
pyq describe make_user        # signature + callers + callees + reaching tests, in one pack
pyq mock-targets              # resolve every mock.patch("…") — flag drifted paths
pyq hierarchy Animal          # supertypes, subclasses, and the override map
pyq deadcode                  # callables reachable from no entrypoint (candidates)
pyq canonical                 # most-used helpers, untested public surface, test inventory
pyq inputs                    # the app's external input surface (env/config/settings)
pyq inputs backfill_calls     # one script's own inputs (CLI args, env it reads)
pyq imports pkg.models --reverse   # who imports pkg.models (blast radius)
pyq imports --cycles          # import cycles among project modules
pyq index                     # prewarm the cache so later verbs are dirt cheap
pyq index clean               # wipe this repo's cached index
```

### Verbs

| Verb | Answers |
|------|---------|
| `refs <symbol>` | Every reference (reads, writes, calls) to a symbol, cross-file. |
| `callers <symbol>` | Every call site of a symbol. |
| `defs <symbol>` | Every definition (function/class/variable/import binding), each tagged `role` (`definition`/`binding`); a `binding` points at its canonical def via `resolves_to`. |
| `graph <symbol>` | The transitive call graph: everything the symbol calls (forward closure), or — with `--reverse` — everything that calls it. Nodes are stable fully-qualified IDs (`pkg.models.User.__init__`) re-queryable after edits; `--depth N` caps the hops. |
| `effects [symbol]` | The transitive effect surface (`fs`, `network`, `subprocess`, `env`, `db`, `random`, `clock`, `global`) of a symbol — or the whole project — **fused with a runtime ledger**: every row is labelled `confirmed` / `predicted` / `observed` / `unverifiable`. Runs the test suite on a cache miss to verify (`PYQ_NO_SUITE` skips). "Is this pure / what does it really touch." See [Effects](#effects). |
| `tests [symbol]` | With a symbol: a call-reachability lens (**not** a coverage metric) — the collected tests structurally wired to it via the reverse call graph, each with `via` + `depth`. With `--base <ref>` (no symbol): the **runtime** oracle — which changed lines the suite actually covers, and by which tests (the absorbed `change-coverage`; runs your tests, Python 3.12+). A test is a `test_*` function in `test_*.py`/`*_test.py`, or a `test_*` method on a collected class (`Test*`-named or `*TestCase`-subclassing). See [Tests](#tests). |
| `describe <symbol>` | One compact context pack for a symbol — its signature (with the **runtime-observed return type** beside it, the absorbed `shapes`), decorators, docstring line, and def line-span, plus its **immediate** callers and callees and the collected tests that reach it. The token-frugal "tell me about X" in a single envelope. Rows carry a `role` (`definition`/`caller`/`callee`/`test`). Runs the suite on a cache miss for the observed type (`PYQ_NO_SUITE` skips). See [Describe](#describe). |
| `mock-targets` | Resolve every `mock.patch("a.b.c")` target against the project and flag *drifted* paths — a patch whose looked-up name no longer exists silently no-ops, so the test passes while exercising the real code. |
| `hierarchy <class>` | The class's supertypes (bases, external marked), transitive subclasses, and the override map — which base methods it overrides and which subclasses override its methods. Resolved across files by ty, subclasses computed by inverting the supertype graph. The OO-refactor footgun, as data. |
| `deadcode` | Functions/classes reachable from **no** entrypoint — candidate dead code, via forward reachability over the call graph. Roots are everything the runtime/framework enters without a project call: tests, dunders, decorated hooks, `__all__`, module-scope calls, entrypoint files (`manage.py`/`wsgi.py`/`urls.py`/`migrations/`/`management/commands/`/…), framework base classes (`BaseCommand`/`*View`/`*Serializer`/…), and `[project.scripts]`. Over-approximate liveness (so it under-reports death); residual dynamic dispatch is flagged. See [Dead code](#dead-code). |
| `canonical` | The repo's canonical surface in one pass: the **most-used** helpers (internal callables ranked by how many distinct non-test callers reach them — what to reach for, not reinvent), the **untested-public** surface (top-level public functions/classes no collected test statically reaches), and the **test** inventory (every collected test with its markers). Rows carry a `section`. The project-level "tell me about this codebase." Same dynamic-dispatch blind spot as the call graph (it cuts both ways). See [Canonical](#canonical). |
| `inputs [script]` | What the code needs to run: env vars / config reads, literal files opened, CLI args (argparse/click), pydantic settings fields. With no argument, the **app** surface — config the running service reads (settings, services, models) — with per-script inputs (Django management commands, `scripts/`, `__main__`-guarded files) held back behind a one-line hint. Pass a script's command name or path (`inputs backfill_calls`) for that script's own inputs. Env detection follows the wrapper conventions real codebases use, not just `os.getenv`/`os.environ`: `get_env`/`get_var`-style helpers, [django-environ](https://github.com/joke2k/django-environ) (`env("X")`, `env.str/bool/int/…`), and [python-decouple](https://github.com/HBNetwork/python-decouple) (`config("X")`). See [Inputs](#inputs). |
| `imports [module]` | The import graph. No arg: every edge. With a module: what it imports; `--reverse`: who imports it (blast radius); `--cycles`: import cycles. Accepts a module name or a file path. |
| `index` | Build the analysis cache for this repo up front (parse + call graph) so later verbs replay from `~/.pyq` instead of reconstructing ty — the first run pays, the rest are dirt cheap. Idempotent. `index clean` removes this repo's cached index. |

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
closure of a symbol (or the whole project, with no symbol) and, for every
reachable callable, scans its body for side-effecting calls — `open`/`shutil`
(`fs`), `requests`/`httpx`/`socket` (`network`), `subprocess`/`os.system`
(`subprocess`), `os.getenv`/`os.environ` (`env`), `*.execute`/`sqlite3.connect`
(`db`), `random`/`secrets`/`uuid` (`random`), `time`/`datetime.now` (`clock`),
and `global` declarations (`global`). Each hit is attributed to the FQN that
actually performs it, so a "pure-looking" entry point reveals the network call
three hops down.

Static detection alone is over-approximate (a hit means "appears to") and blind
to dynamic dispatch (it misses effects behind `getattr`/attribute calls). So
`effects` **fuses the static surface with a runtime ledger** — it runs the test
suite (once, then cached under `~/.pyq`; `pyq index` pre-warms it, `PYQ_NO_SUITE`
skips it) and labels every row by **confidence**:

- **`confirmed`** — static predicted it *and* the suite performed it.
- **`predicted`** — static says so, the run didn't exercise it (over-approximation
  *or* simply uncovered).
- **`observed`** — the run performed it but static missed the edge (dynamic
  dispatch) — the effect the static surface structurally can't see.
- **`unverifiable`** — a category the audit hook can't watch (`env`-read,
  `random`, `clock`, `global`).

```console
$ pyq effects --root path/to/project
effects: 1 confirmed, 1 observed, 1 predicted, 1 unverifiable
confirmed (1)
  pkg/ops.py:3:12  fs  open  pkg.ops.confirmed_fs
observed (1)
  subprocess    pkg.ops.dynamic_only_subprocess   (static missed this edge)
predicted (1)
  pkg/ops.py:8:9  network  socket.socket  pkg.ops.static_only_net
unverifiable (1)
  pkg/ops.py:10:12  env  os.getenv  pkg.ops.reads_env
```

This is the absorbed `effect-diff`: there's no separate verb — the join *is* what
`effects` returns. When the suite can't run (no interpreter/pytest, or
`PYQ_NO_SUITE`), every row degrades to `predicted`/`unverifiable` with a note,
never an error. Module- and class-body effects are still flagged `import-time`.
Only `confirmed` is proof the effect runs; `predicted` is a candidate to verify.

## Tests

`tests` has two modes. With a **symbol** it's the static reaching-tests map
(below). With **`--base <ref>`** and no symbol it's the runtime oracle — the
absorbed `change-coverage`: it runs the suite under per-test line coverage and
reports which lines changed since `<ref>` were actually executed, and by which
tests (Python 3.12+ for real line coverage; degrades on older). Use it to answer
"did my edit land on a covered line?"

```console
$ pyq tests --base main --root path/to/project
change-coverage vs main: 3/4 changed lines covered, 1 uncovered across 2 file(s)
pkg/core.py:42  covered    tests.test_core.test_parse
pkg/core.py:58  uncovered
```

The symbol form is a reverse-call-graph projection: it walks the closure of
callers of a symbol and keeps the ones a test runner would collect — `test_*`
functions in
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

## Describe

`describe` answers "tell me about X" in one round-trip. Instead of running
`defs`, then `callers`, then `graph`, then `tests` and stitching the answers
together, it packs a symbol's static facets and its immediate neighbourhood into
a single envelope:

- **Definition** — signature (parameters + return annotation; for a class, its
  bases), decorators as written, the first docstring line, and the def's line
  span (`[first, last]`), read straight off the syntactic index — plus the
  **runtime-observed return type** beside the declared one (`observed int | str`,
  the absorbed `shapes`) when the suite has run. Runs the suite on a cache miss
  to collect it (Python 3.12+; `PYQ_NO_SUITE` skips), so on a cold repo the
  observed type may be absent until the first run.
- **Immediate callers / callees** — the depth-1 call graph in both directions
  (what it calls in one hop, who calls it in one hop).
- **Reaching tests** — the collected tests that reach it transitively, each with
  the call path (`via`) and `depth` — the same lens as the `tests` verb.

```console
$ pyq describe make_user --root path/to/project
describe `make_user`: 1 def, 1 immediate caller, 1 callee, 1 reaching test
pkg/models.py:5:5         def make_user(name: str) -> User  [L5-6]  — Build a user.
app.py:3:5                caller app.main
pkg/models.py:1:7         callee pkg.models.User
tests/test_users.py:8:5   test tests.test_users.test_make_user (depth 2, via app.main)
```

Every row carries a `role` (`definition`/`caller`/`callee`/`test`); the
definition row also exposes `signature`, `decorators`, `doc`, and `lines` as
structured fields under `--json`. The neighbourhood inherits the call graph's
**dynamic-dispatch blind spot** — callers/callees/tests reached only through
attribute or framework dispatch are not shown (flagged in `warnings`). A name
that resolves to several defs gets one definition row apiece, with the
caller/callee/test sets taken as the union over them (flagged); qualify the name
(`Alpha.process`) to disambiguate.

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

## Canonical

`deadcode` asks "what's reachable from the entrypoints"; `canonical` asks three
questions an agent has *before* it edits an unfamiliar repo, and answers them in
one pass. Each row carries a `section`:

- **`most-used`** — internal callables ranked by **use**: the count of distinct
  callers *defined outside the test tree* (a projection of the call graph's
  in-degree). These are the utilities to reach for instead of reinventing. A
  helper used in ≥2 non-test places is shown, top 30 by use; candidates defined
  in the test tree (fixtures/factories) or an entrypoint file (`scripts/`,
  `manage.py`, migrations, management commands) and dunder plumbing are excluded
  — those are glue, not utilities to reach for.
- **`untested-public`** — the public surface (top-level, non-`_` functions and
  classes) that **no collected test statically reaches** and that the framework
  doesn't drive. Same reachability machinery as `deadcode`, seeded from the test
  set instead of the entrypoints (override edges included, so a symbol reached
  only polymorphically from a test still counts as tested); then the same
  framework entrypoints `deadcode` treats as live — serializers, configs,
  migrations, commands, routers, decorated handlers, string-config targets — are
  subtracted, since a test rarely *calls* those directly and leaving them in
  buries the real gaps under framework classes exercised through dispatch. (On
  real Django repos this cut the list ~80–90%, e.g. 835→89, leaving the plain
  untested service/helper functions.)
- **`test`** — the test inventory: every pytest-collected test, with the markers
  read off its own and its enclosing class's decorators (`@pytest.mark.slow`,
  `parametrize`, a class-level `django_db` inherited by each method).

```console
$ pyq canonical --root path/to/project
canonical: 1 most-used helper, 3 untested public symbols, 3 collected tests
pkg/core.py:6:5    most-used pkg.core.normalize (used by 3)
pkg/core.py:14:5   untested-public pkg.core.parse_title
pkg/core.py:18:5   untested-public pkg.core.parse_tag
pkg/core.py:26:5   untested-public pkg.core.untested_public
tests/test_core.py:12:5  test tests.test_core.test_param [parametrize]
tests/test_core.py:7:5   test tests.test_core.test_tested_public [slow]
tests/test_models.py:8:9 test tests.test_models.TestModels.test_one [django_db]
```

The call graph's **dynamic-dispatch blind spot cuts both ways** here, and that's
flagged in `warnings`: a helper reached only through attribute/framework
dispatch is *undercounted* in `most-used`. Subtracting the framework entrypoints
removes the bulk of the false `untested-public` (the serializers/configs/tasks a
test never calls directly), but a symbol reached only through *other* dynamic
dispatch can still be flagged though it runs at runtime. So "untested" means "no
*static* reaching test," **not** "uncovered" — `tests --base` is the runtime
oracle for coverage. Test
collection follows the same pytest + unittest/`TestCase`-inheritance rules as the
`tests` verb (custom `python_files`/`python_classes` config isn't read); markers
come from decorators, so a module-level `pytestmark` isn't captured.

## Inputs

`inputs` answers "what does this need to run" as relational facts: the env vars
and config keys it reads, literal files it opens, CLI args it declares, and
pydantic `BaseSettings` fields. Like `imports`, it's a **pure syntactic,
over-approximate** scan — a computed key/path buckets to `<dynamic>` rather than
guess, and a match means "appears to read this," not a proof.

**App vs. script.** A bare `pyq inputs` reports the **app** surface — the inputs
of the long-running service: settings modules, services, models, views. The
per-script inputs that would otherwise drown it (a Django management command's
`--dry-run`, a one-off in `scripts/`) are held back behind a one-line hint, and
shown only when you name that script: `pyq inputs backfill_calls` (by command
name) or `pyq inputs path/to/cmd.py` (by path). A file is treated as a *script*
when it's a Django management command, lives in `scripts/`/`bin/`, is
`manage.py`, or carries an `if __name__ == "__main__":` guard. Tests are excluded
from the default view.

**Env detection follows wrappers.** Real codebases rarely call `os.getenv`
directly — they funnel config through a helper (`var_provider.get_var("DB_URL")`)
or a library. `inputs` recognizes the conventions by call shape (a config-accessor
name with a string-literal first argument), so the wrapped surface is visible,
not just the stdlib idiom:

- `os.getenv` / `os.environ[...]` / `.get` / `"X" in os.environ` (stdlib)
- `get_env` / `get_var` / `getvar` — provider-style and `env_utils.get_env` helpers
- [django-environ](https://github.com/joke2k/django-environ): `env("X")`, `env.str/bool/int/float/list/json/db/url/...("X")`
- [python-decouple](https://github.com/HBNetwork/python-decouple): `config("X")`

The string-literal-first-argument guard keeps argless lookalikes
(`get_config()`) out, and the match is over-approximate by design (a generic
`obj.config("x")` will register) — in keeping with the verb's contract.

```console
$ pyq inputs --root path/to/django-project
118 app inputs

env (115)
  salessync/settings.py:38:10   DB_NAME
  salessync/settings.py:40:10   DB_PASSWORD
  salessync/settings.py:73:10   SECRET_KEY
  salessync/settings.py:91:18   STRIPE_SECRET_KEY
  ...

setting (3)
  ...

notes
  14 scripts have their own inputs (management commands, scripts/) — query one by name:
  pyq inputs <name> — e.g. backfill_calls, seed_ppc_data, ppc_spend_coverage, …
```

## Output envelope

Every verb emits the same structured envelope; two renderers project it.

**Human view (default, even when piped).** A summary header, then results
grouped into blank-line-separated **sections** (the classifier each row would
otherwise repeat — the effect category, the role, the depth ring — hoisted to a
`section (count)` header), with `loc` and the remaining fields aligned into
columns. Warnings collect under a trailing `notes` block. The full clickable
`path:line:col` stays on every row.

```console
$ pyq inputs --root examples/sample
11 app inputs

env (4)
  config.py:3:9   DEBUG
  config.py:4:10  DATABASE_URL
  config.py:5:11  TIMEOUT
  config.py:7:10  <dynamic>

file (1)
  config.py:10:10  settings.ini

setting (2)
  cli.py:7:5  db_url
  cli.py:8:5  port

arg (4)
  cli.py:14:5  --verbose
  ...
```

**`--json` (opt-in).** The structured envelope — stable, presentation-agnostic.
Each row carries its semantic fields plus presentation hints (`group`, `cols`)
the human renderer uses; consumers read the typed fields and ignore the rest.

```json
{
  "tool": "pyq",
  "query": { "kind": "inputs", "target": null },
  "summary": "11 app inputs",
  "count": 11,
  "results": [
    { "loc": "config.py:3:9", "label": "env DEBUG", "group": "env", "cols": ["DEBUG"] },
    { "loc": "cli.py:14:5",   "label": "arg --verbose", "group": "arg", "cols": ["--verbose"] }
  ]
}
```

Locations are `path:line:col` (1-based, UTF-8 character columns).

## Workspace

| Crate | Role |
|-------|------|
| `pyq-cli` | clap front end; verb-per-invocation; `.gitignore`-respecting tree walk. |
| `pyq-index` | One parse per file → `FileIndex` of defs/refs/inputs/effects. Backs the syntactic path. |
| `pyq-resolve` | Cross-file resolution behind a `Resolver` trait, plus the transitive `CallGraph`, backed by ty. All ty contact lives here. |
| `pyq-output` | The shared envelope and its human / `--json` / `--pretty` renderers. |

See [DESIGN.md](DESIGN.md) for the thesis, the roadmap, and the
ty-vs-`ruff_python_semantic` decision.

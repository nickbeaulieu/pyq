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
| `pyq-index` | One parse per file → `FileIndex` of defs/refs/inputs. Backs the syntactic path. |
| `pyq-resolve` | Cross-file resolution behind a `Resolver` trait, backed by ty. All ty contact lives here. |
| `pyq-output` | The shared envelope and its human / `--json` / `--pretty` renderers. |

See [DESIGN.md](DESIGN.md) for the thesis, the roadmap, and the
ty-vs-`ruff_python_semantic` decision.

# pyq

[![ci](https://github.com/nickbeaulieu/pyq/actions/workflows/ci.yml/badge.svg)](https://github.com/nickbeaulieu/pyq/actions/workflows/ci.yml)

A queryable code graph for Python. Ask who calls what, and what a symbol touches; get back a clean terminal view or structured JSON — instead of grepping and guessing.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/nickbeaulieu/pyq/main/install.sh | sh
```

Downloads the right prebuilt binary for your platform (macOS and Linux, x86_64
and arm64), verifies its checksum, and installs to `~/.local/bin`.

### From source
```bash
./install.sh --dir=~/.local/bin
```

### Channels & upgrading

**stable** tracks tagged releases; **canary** rolls forward with every push to `main`.

```bash
pyq channel            # show the current channel and build
pyq channel canary     # switch channels (run upgrade to actually move)
pyq upgrade            # update in place to the latest build on your channel
pyq upgrade --check    # see what an upgrade would do, without installing
```

### Build from source

```bash
cargo build --release      # binary at target/release/pyq
```

Needs the Rust toolchain pinned in `rust-toolchain.toml`. The first build
fetches and compiles a pinned `ruff`/`ty`, so it takes a while.

## Usage

One verb per invocation: `pyq <verb> [args] [flags]`.

```bash
pyq refs User                 # every reference to `User` across the tree
pyq callers make_user         # every call site of `make_user`
pyq defs User                 # every definition of `User`
pyq graph main                # everything `main` transitively calls
pyq graph User --reverse      # everything that transitively reaches `User`
pyq effects handle_request    # side effects it performs (io/net/db/…)
pyq tests add                 # which tests reach `add` (run before editing)
pyq tests --base main         # which changed lines your suite covers
pyq describe make_user        # signature + callers + callees + tests, in one pack
pyq mock-targets              # find mock.patch("…") targets that no longer resolve
pyq hierarchy Animal          # parents, subclasses, and the override map
pyq deadcode                  # callables no entrypoint reaches (candidates)
pyq canonical                 # most-used helpers, untested public API, test inventory
pyq inputs                    # what the app reads to run (env/config/settings)
pyq inputs backfill_calls     # one script's own inputs (CLI args, env)
pyq imports pkg.models --reverse   # who imports pkg.models (blast radius)
pyq imports --cycles          # import cycles among project modules
pyq index                     # prewarm the cache so later verbs are fast
pyq index clean               # wipe this repo's cached index
```

### Verbs

| Verb | What it does |
|------|--------------|
| `refs <symbol>` | Every reference (reads, writes, calls) to a symbol, cross-file. |
| `callers <symbol>` | Every call site of a symbol. |
| `defs <symbol>` | Every definition of a symbol (function, class, variable, import). |
| `graph <symbol>` | The transitive call graph: what the symbol calls, or — with `--reverse` — what calls it. `--depth N` caps the hops. |
| `effects [symbol]` | The side effects a symbol — or the whole project — performs (`fs`, `network`, `subprocess`, `env`, `db`, `random`, `clock`, `global`), each labelled by whether your tests confirm it. |
| `tests [symbol]` | With a symbol: which tests reach it (and the call path) — run these before editing. With `--base <ref>`: which changed lines your suite actually covers. |
| `describe <symbol>` | One pack for a symbol: signature, immediate callers and callees, and reaching tests. |
| `mock-targets` | `mock.patch("…")` targets that no longer resolve — patches that silently do nothing. |
| `hierarchy <class>` | A class's parents, subclasses, and override map. |
| `deadcode` | Functions and classes no entrypoint reaches — delete candidates (verify first). |
| `canonical` | Repo overview: most-used helpers, untested public API, and the test inventory. |
| `inputs [script]` | What the code reads to run: env vars, config, files opened, CLI args, settings. Name a script for its own inputs. |
| `imports [module]` | The import graph: edges, who imports a module (`--reverse`), or cycles (`--cycles`). |
| `index` | Prewarm the cache so later verbs are fast. `index clean` wipes it. |

### Flags

Global — accepted before or after the verb.

| Flag | Effect |
|------|--------|
| `--root <dir>` | Directory to scan (default: current directory). |
| `--json` | Compact single-line JSON. |
| `--pretty` | Indented JSON. |

### Good to know

- **Run from your package root** (`--root`) — the directory whose children are
  your top-level packages — or absolute imports won't resolve and results will
  be under-reported.
- **`effects`, `describe`, and `tests --base` run your test suite once**, then
  cache it under `~/.pyq` (`pyq index` prewarms it; `PYQ_NO_SUITE` skips it).
  `tests --base` and runtime return types need Python 3.12+.
- **Reverse-reachability is static.** `callers`, `tests`, and `graph --reverse`
  follow call edges, not dynamic dispatch — so a framework-driven symbol (a
  view, a signal handler) can show `0` while being called constantly. When that
  applies the output says so (a note + `caveat` in JSON); confirm runtime
  coverage with `pyq tests --base`.
- **`deadcode` and untested-public are candidates, not facts** — over-approximate
  by design. Verify before deleting.

## Output

Every verb emits the same envelope. The default human view is a summary header,
then results grouped into aligned sections, with a clickable `path:line:col` on
each row and any warnings under a trailing `notes` block.

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

arg (4)
  cli.py:14:5  --verbose
  ...
```

`--json` emits the structured envelope — stable and presentation-agnostic.
Consumers read the typed fields and ignore the rest.

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

---

See [DESIGN.md](DESIGN.md) for the thesis, the design, and the roadmap.

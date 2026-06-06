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
```

### Verbs

| Verb | Answers | Engine |
|------|---------|--------|
| `refs <symbol>` | Every reference (reads, writes, calls) to a symbol, cross-file. | ty |
| `callers <symbol>` | Every call site of a symbol. | ty |
| `defs <symbol>` | Every definition (function/class/variable/import binding). | ty |
| `inputs` | What the code needs to run: env vars, literal files opened, CLI args (argparse/click), pydantic settings fields. | syntactic |

### Flags

All flags are global (accepted before or after the verb).

| Flag | Effect |
|------|--------|
| `--root <dir>` | Directory to scan. Defaults to the current directory. |
| `--json` | Emit the compact single-line JSON envelope. |
| `--pretty` | Emit indented JSON. |
| `--syntactic` | Force the single-file syntactic extractor instead of ty (for `refs`/`callers`/`defs`). Faster, no project database, name-matched within a module — a fallback / comparison path. |

## Two engines: ty vs. syntactic

- **ty (default for `refs`/`callers`/`defs`)** — drives ty's project-wide
  semantic engine for real cross-file binding through imports, re-exports, and
  aliasing. All ty contact is confined to the `pyq-resolve` crate.
- **syntactic (`--syntactic`, and always for `inputs`)** — one parse per file
  via `ruff_python_parser`, matching names within a single module. Parse errors
  are non-fatal, so a half-edited file still answers. No project database.

`inputs` is intentionally syntactic and over-approximate: computed env keys or
paths bucket to `<dynamic>` rather than guess, and CLI-arg detection is
suffix-matched (`.add_argument`/`.option`/`.argument`).

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

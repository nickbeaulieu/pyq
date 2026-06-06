# pyq — bugs & improvements

Found by exercising the release binary against a large Django repo (`scoring`,
~1900 `.py` files, contains a nested git worktree at `.claude/worktrees/`).

Severity: **P1** correctness, **P2** misleading output, **P3** UX/minor.

(Also exercised against `mroi-matcher` — a `src`-rooted package whose first-party
imports use bare module names via `pythonpath = ["mroi_matcher"]`.)

---

## ✅ FIXED — ty under-reports source-root (bare-path) first-party imports
`mroi-matcher` declares `pythonpath = ["mroi_matcher"]`, so first-party code
imports by bare name (`from helpers.validators import valid_email`). ty discovers
the project at the repo root and previously didn't honor that source root, so the
affected symbols returned def-only / 0 (used code looked dead).

**Fixed natively** (`ty_backed.rs`): on project init, pyq reads
`[tool.pytest.ini_options] pythonpath` from the project's `pyproject.toml` and
feeds those dirs to ty as `environment.extra-paths` via `apply_overrides`
(additive — ty keeps its own auto-detected `./src`/`./<project>` roots and the
project root). ty now resolves the bare imports itself, so results are
**ty-resolved**, not syntactic name-matches — precision restored, no warning.

```
                     OLD (ty alone)   INTERIM (unified backfill)   NOW (ty resolves)
valid_email refs     1 (def only)     4 (3 syntactic-only)         7  ✓ all ty
valid_email callers  0                3 (syntactic-only)           3  ✓ all ty
get_score  callers   0                0 (method via match_case)    2  ✓
get_matches callers  0                0                            2  ✓
```

Verified: no-`pythonpath` repos unchanged (scoring `save`=62, `__init__`=37;
alice reverse=138); missing/broken/`.`-only `pythonpath` handled gracefully
(no crash). Best-effort: parse failure → no extra paths, never errors the query.
(`[tool.ty] extra-paths`/`src.root` and `./src`/`./<project>` auto-detect are
already honored by ty directly; a `--src-root` CLI flag remains a possible
future nicety but isn't needed for the common pytest-`pythonpath` convention.)

(The related `imports`-graph forward/reverse identity mismatch is also fixed:
targets canonicalize to the file-derived module id, so `main.models` and
`alice.main.models` — and both directions — resolve to one node.)

---


## P3 (residual) — dotted/qualified names accept the qualifier but ignore it
Re-verifying the old "dotted names return 0" fix: `defs A.proc` and `defs B.proc`
now return results (good, no longer 0) — but they return *identical* results
(both `A.proc` and `B.proc` defs). The qualifier is dropped and only the last
component (`proc`) is matched, so `A.proc` does not actually scope to `A`. It
traded "returns 0" for "silently ignores the prefix," which can mislead an agent
that believes it targeted one class. Practical disambiguation does exist via the
new `resolves_to` field on `callers`/`refs` results (each call site is tagged
with the def it resolves to — verified working), so this is low priority; but
either honor the qualifier or reject/warn on it rather than silently widen.

## Design preferences (agent POV)
What I'd want as the actual consumer. Principle: **pyq output is treated as
ground truth, so every divergence from reality costs more than slowness would.**
Optimize for "an agent can act on this without double-checking."

1. ✅ **Done — one notion of `def`, with a `role` field, not two disagreeing
   engines.** `refs`/`callers`/`defs` now run a single `unified` engine (ty ∪
   syntactic). Every result is tagged `role` (`definition`/`binding`/
   `reference`/`call`) and `source` (`ty`/`syntactic`), and a `binding` carries
   `resolves_to` the canonical def:
   ```json
   {"loc":"pkg/models.py:5:5","label":"def","role":"definition","source":"ty"}
   {"loc":"app.py:1:30","label":"import","role":"binding","source":"syntactic",
    "resolves_to":"pkg/models.py:5:5"}
   ```
   The 1-vs-36 split is one answer the caller filters (`role=="definition"`).
   `--syntactic` is now a debug filter, not a semantic fork. Function-local
   variables (ty's blind spot) are filled from the syntactic scan and flagged
   via a `warnings` entry, so a `0` is no longer silently wrong.

2. ✅ **Done — determinism independent of cwd.** Every result path is anchored
   to the canonical project root, and that resolved absolute root is echoed in
   the envelope (`"query":{...,"root":"/abs/scoring","engine":"unified"}`), so
   the same logical query gives the same answer and the same paths from any
   working directory.

3. ✅ **Done — `--root` always means scope.** ty inherits the CLI walk's
   discipline: emitted results are filtered to the walked file set under
   `--root`, honoring `.gitignore`/hidden filtering (no nested-worktree
   double-counts) and anchoring paths to the resolved root.

4. ✅ **Done (mechanism) — structured `warnings` array.** The envelope carries a
   `warnings` list; syntactic-only unified results and the `--syntactic`
   attribute blind spot are flagged today. More producers (unresolved dynamic
   imports, nested-worktree notes) can append to the same channel — the
   `inputs` `<dynamic>` bucketing instinct, surfaced everywhere.

5. ✅ **Done — counts you can branch on.** Results are de-duplicated by location
   and scoped to `--root`, and ty's nested-worktree double-counting is gone, so
   the headline integer ("0 callers → safe to delete") is trustworthy.

---

## Confirmed working
- `callers` labels each call site with its enclosing function (as advertised).
- Output is deterministic across runs.
- Bad `--root` errors cleanly (exit 1, readable message).
- `imports` honors `--root` (it's syntactic): `--root billing` → 224 edges,
  scoped; full repo → 4143. Internal-vs-`(ext)` classification looks right.
- `imports` relative-import resolution is correct when targets exist: `from .`,
  `from .pkg`, and `from .. import x` all map to the right internal module, and
  reverse-deps attribute them correctly. (Feeding invalid Python — a relative
  import past the top-level package — yields a junk `(ext)` target, but that's
  invalid input, not a bug.)
- `imports --cycles` and full-graph build are fast (~0.1-0.25s on ~1900 files).
- `refs` read/write classification is good: tags writes vs reads, splits
  `counter = counter + 1` into a write (LHS) + read (RHS), and labels `global x`
  as a generic `ref`. Beyond grep.
- `callers` is alias-aware: a call through `import as`/re-export resolves back to
  the origin (`make_widget()` and aliased `mw()` both attributed to
  `make_widget`). Re-export through `__init__.py` resolves correctly; `defs`
  points to the single origin. `refs` now folds these call sites in too
  (`callers ⊆ refs`).
- Decorators & `@property`: decorator application (`@my_decorator`) counts as a
  caller; property access (`c.value`) is a read; calls to a decorated method
  resolve to its def.
- Star-imports (`from lib import *`) and `try/except ImportError` conditional
  imports resolve correctly — call sites found, both candidate defs listed.
- `inputs` detects pydantic `BaseSettings` fields (`setting db_url`, `setting
  port`).
- Column numbers are true Unicode **codepoints**, not bytes or UTF-16 units —
  verified with multibyte (`é`) and astral (`🎉`) chars on the same line. (Many
  LSP-backed tools leak UTF-16 here; pyq gets it right.)

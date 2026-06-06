# pyq — bugs & improvements

Found by exercising the release binary against a large Django repo (`scoring`,
~1900 `.py` files, contains a nested git worktree at `.claude/worktrees/`).

Severity: **P1** correctness, **P2** misleading output, **P3** UX/minor.

(Also exercised against `mroi-matcher` — a `src`-rooted package whose first-party
imports use bare module names via `pythonpath = ["mroi_matcher"]`.)

---

## ~MOSTLY FIXED via unified engine — ty under-reports source-root (bare-path) imports
`mroi-matcher` declares `pythonpath = ["mroi_matcher"]`, so first-party code
imports by bare name (`from helpers.validators import valid_email`). ty discovers
the project at the repo root, doesn't honor that source root, and on its own
returned def-only / 0 for the affected symbols (used code looked dead):

```
                     OLD (ty)            NOW (unified, ty ∪ syntactic)
valid_email refs     1 (def only)        4  ✓
valid_email callers  0                   3  ✓   (grep: 3 real calls)
```

**The `unified` default engine fixes the observable symptom** — syntactic
name-matching backfills the call sites ty's unresolved imports miss, so `refs`/
`callers` are no longer silent-zero here. Verified on the current binary.

Residual (lower priority): the underlying ty *resolution* gap remains — unified
papers over it by name (so it inherits name-collision imprecision rather than
true resolution), and `settings` vs `helpers.validators` resolving inconsistently
in ty is still a smell. Honoring a real source root (`[tool.pytest.ini_options]
pythonpath`, `[tool.ty] extra-paths`/`src.root`, `src`-layout auto-detect, or a
`--src-root` flag) would let ty resolve these natively and restore precision.
(The related `imports`-graph forward/reverse identity mismatch is now fixed:
targets canonicalize to the file-derived module id, so `main.models` and
`alice.main.models` — and both directions — resolve to one node.)

---


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

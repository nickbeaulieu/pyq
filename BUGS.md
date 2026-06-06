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

## P3 — JSON envelope `query` block isn't fully uniform across verbs
The queried target is now echoed uniformly as `query.target` (refs/callers/defs
and `imports <module>`), and `engine` is present on every resolver verb. Residual:
```
refs/callers/defs : query = {engine, kind, target}
imports <module>  : query = {kind, mode, target, found}   # no engine
inputs / imports  : query = {kind[, mode]}                # no target/engine — none to report
```
`engine` is absent on `imports`/`inputs` (they're pure syntactic facts, not a
resolver query) and `target` is absent where there is none. Tolerable, but for a
fully uniform schema an agent can branch on blindly, consider emitting `engine`
everywhere (`"syntactic"` for the fact verbs) and a `null` `target`.

---

## P3 — `--syntactic` debug filter still can't see attribute-access call sites
The syntactic scan matches only bare `Name` nodes, never `Attribute` access
(`obj.method()`), so `--syntactic callers save` → 0 where the merged default
(ty) finds 62. This is no longer the dangerous silent-zero it was: the default
engine is now `unified` (ty ∪ syntactic), so attribute calls *are* found by
default, and `--syntactic` is documented as a debug filter (ty skipped), not a
grep-replacement. Residual nicety: when `--syntactic` answers `refs`/`callers`
for a name only ever used via attribute access, emit a warning rather than a
bare 0, since the debug path itself has no over-approximation flag.

## P2 — `callers`/`refs` union over same-named defs with no way to target one
Two unrelated classes each defining `process`:

```python
class Alpha:
    def process(self): ...
class Beta:
    def process(self): ...
a.process(); b.process()
```
```
defs    process  → 2   (Alpha.process, Beta.process)
callers process  → 2   (a.process(), b.process())  — merged, both labeled scope `m`
```

A bare-name query collapses all defs of that name: `callers process` returns
Alpha's *and* Beta's call sites with no indication of which `process` each site
resolves to. For the core refactor use case ("who calls `Alpha.process` so I can
rename it") this conflates unrelated methods — an agent would wrongly think
`b.process()` also needs updating. ty's `call_hierarchy` is anchored per-def and
*knows* which target each call resolves to; pyq unions the results and discards
that. Two fixes, ideally both: (a) label each caller/ref with the def it resolves
to (`→ Alpha.process`), and (b) allow targeting a single def (qualified name
`Alpha.process`, or a `path:line` anchor) — note dotted names currently return 0
(see P3), so there's *no* precise-targeting path today.
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

2. **Determinism independent of cwd.** Same logical query → same answer from any
   directory. Discover the project once, anchor everything to that root, and put
   the resolved root in the envelope:
   `"query":{...,"root":"/abs/scoring","engine":"ty"}`.

3. **`--root` always means scope.** If scoping can't be done cheaply for ty,
   **fail loud** (`error: --root unsupported with ty engine`) rather than
   silently ignore it. Silent ignore is the worst option — I'll believe the
   scope held. Honor the ignore-walk (skip hidden/worktree copies) for both
   engines.

4. **Surface what you couldn't do** via a structured `warnings`/`notes` array:
   `"scanned 2 project roots; .claude/worktrees/… looks like a nested copy"`,
   `"3 references unresolved (dynamic import)"`, `"env key computed → <dynamic>"`.
   `inputs`'s `<dynamic>` bucketing is exactly the right instinct — surface it
   everywhere as structured warnings, not just inline. Over-approximate-and-flag
   beats silently-precise-and-wrong: the flag tells me when to fall back to
   reading the file myself.

5. **Counts I can branch on.** The headline value over grep is "0 callers → safe
   to delete" / "47 callers → big blast radius." I act on the integer before
   reading the list, so it must be de-duplicated and scoped. A doubled worktree
   count silently turns a safe refactor into wasted re-reading, or makes dead
   code look used.

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

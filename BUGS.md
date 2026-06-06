# pyq — bugs & improvements

Found by exercising the release binary against a large Django repo (`scoring`,
~1900 `.py` files, contains a nested git worktree at `.claude/worktrees/`).

Severity: **P1** correctness, **P2** misleading output, **P3** UX/minor.

---

## P1 — `--root` is silently ignored by the ty engine
`refs`/`callers`/`defs` route through ty, which does its own project discovery
and scans the **entire** discovered project regardless of `--root`.

```
pyq defs __init__ --root billing   → 74 results (incl. api/, call_log/, .claude/…)
pyq defs __init__ --root .         → 74 results (identical)
```

The syntactic engine honors `--root` correctly (`--root billing` → 2,
`--root .` → 37). README advertises `--root` as "Directory to scan" with no
caveat; for the default (ty) verbs it does nothing.

Fix: scope ty's file set / results to the subtree, or document that `--root`
only affects the syntactic path.

---

## P1 — ty engine bypasses the `.gitignore`/hidden-dir walk → double-counted results
The CLI's `ignore`-based walk skips hidden dirs; ty does not. The repo has a
nested worktree at `.claude/worktrees/ppc-…/` (full copy of the project). ty
scans it, so every symbol is double-reported.

```
pyq defs __init__               (ty)  → 74  (37 real + 37 worktree duplicates)
pyq defs __init__ --syntactic         → 37  (correct — walk skips hidden .claude)
```

`defs Call` reports "2 defs" that are the *same* def in two copies. Inflated
counts mislead an agent treating output as ground truth. Same root cause as the
`--root` bug: ty isn't fed the CLI's filtered file list.

---

## P2 — Path format is inconsistent within one result set
Paths under cwd are relativized; everything else stays absolute — mixed in the
same output.

```json
"results":[
  {"loc":"/Users/…/.claude/worktrees/…/scoring/models.py:1624:7"},
  {"loc":"models.py:1629:7"}
]
```

Looks like a `strip_prefix(cwd)` that only fires for in-cwd paths. Locations
should be uniformly relative to one anchor (root) or uniformly absolute, so
they are stable/clickable and machine-comparable.

---

## P2 — `inputs` misses `in os.environ` membership tests
`scoring/management/commands/ephemeral_bootstrap_db.py:99`:

```python
if 'DJANGO_SUPERUSER_PASSWORD' not in os.environ:
```

`inputs` catches `os.environ['X']` and `os.environ.get('X')` but not
`'X' in os.environ` / `not in`. A real env dependency the surface misses.

---

## P3 — Qualified/dotted names return 0
`pyq defs scoring.models.Call` → 0, while `pyq defs Call` works. An agent will
naturally reach for the dotted path. Consider stripping to the last component
or matching the qualified form.

## P3 — Empty symbol silently succeeds
`pyq defs ""` → "0 defs of ``", exit 0. Probably should be a usage error.

---

## Through-line
P1–P2 share a cause: the ty path doesn't inherit the CLI's tree-walk discipline
(root scoping + ignore filtering + path anchoring). Routing ty's file set and
result paths through the same walk/normalization the syntactic path already uses
would fix all of them at once.

## P2 — `defs` means different things in each engine (contract violation)
The two engines answer different questions under the same verb:

```
ty  defs Call             → 1   (just `class Call` — the canonical origin)
syntactic defs Call       → 36  (1 class + 35 `from … import Call` bindings)
```

The README defines a def as "function/class/variable/**import binding**." ty
omits import bindings, so the *default* engine under-delivers against its own
spec; syntactic over-delivers vs ty. `--syntactic` is sold as a
"comparison/fallback" — implying same-question-shallower — but it returns a 36×
larger, qualitatively different result set. An agent swapping engines for speed
gets a different answer, not a degraded-same answer.

## P2 — ty result count is cwd-dependent (even though --root is ignored)
```
cd scoring/scoring && pyq defs Call   → 2   (worktree dup)
cd scoring         && pyq defs Call   → 1
```
Same logical query, different counts by invocation dir. ty's discovered project
shifts with cwd; combined with the ignored `--root`, counts can't be trusted.

---

## Design preferences (agent POV)
What I'd want as the actual consumer. Principle: **pyq output is treated as
ground truth, so every divergence from reality costs more than slowness would.**
Optimize for "an agent can act on this without double-checking."

1. **One notion of `def`, with a `role`/`kind` field — not two disagreeing
   engines.** Always return canonical definition(s) *and* import bindings,
   tagged so I can filter:
   ```json
   {"loc":"scoring/models.py:1629:7","kind":"class","role":"definition"}
   {"loc":"ingest/views.py:46:28","kind":"import","role":"binding",
    "resolves_to":"scoring/models.py:1629:7"}
   ```
   The 1-vs-36 split becomes one answer I filter (`role=="definition"` → 1).
   Engine choice should be an implementation detail; `--syntactic` becomes a
   debug flag, not a semantic fork.

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

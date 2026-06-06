# pyq — bugs & improvements

Found by exercising the release binary against a large Django repo (`scoring`,
~1900 `.py` files, contains a nested git worktree at `.claude/worktrees/`).

Severity: **P1** correctness, **P2** misleading output, **P3** UX/minor.

---

## P2 — `inputs` misses `in os.environ` membership tests
`scoring/management/commands/ephemeral_bootstrap_db.py:99`:

```python
if 'DJANGO_SUPERUSER_PASSWORD' not in os.environ:
```

`inputs` catches `os.environ['X']` and `os.environ.get('X')` but not
`'X' in os.environ` / `not in`. A real env dependency the surface misses.

## P2 — `inputs` misses `os.environ.setdefault(...)` — incl. DJANGO_SETTINGS_MODULE
Of 15 env-access sites in the repo, `inputs` caught 7 and missed every
`setdefault`. That pattern appears 5× and sets `DJANGO_SETTINGS_MODULE` at every
entrypoint:

```
manage.py:6              os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'salessync.settings')
salessync/wsgi.py:23     os.environ.setdefault('DJANGO_SETTINGS_MODULE', ...)
salessync/celery.py:12   os.environ.setdefault('DJANGO_SETTINGS_MODULE', ...)
mcp_server/main.py:14    os.environ.setdefault('DJANGO_SETTINGS_MODULE', ...)
celeryhealthcheck/app.py:42  os.environ.setdefault('DJANGO_SETTINGS_MODULE', ...)
```

`DJANGO_SETTINGS_MODULE` is *the* "what does this need to run" env var for a
Django app — exactly `inputs`' purpose — and it's invisible. `setdefault(key,
default)` is a read-with-fallback (semantically like `.get(key, default)`) and
should be captured.

## P3 — `inputs` env detection doesn't follow `from os import environ` alias
`sara/tests/test_handle_mroi_event.py:3048  environ.get('test_case')` is missed.
Detection is attribute-matched on `os.environ`/`os.getenv`; a module imported as
`from os import environ` (or `import os as o`) escapes it. Over-approximate
suffix-matching (any `.environ`/`environ.get`) or tracking the import alias would
close it. (Edge: `env_vars = os.environ` whole-dict binds expose unknown keys —
candidate for an `env <dynamic>` flag.)

---

## P3 — Qualified/dotted names return 0
`pyq defs scoring.models.Call` → 0, while `pyq defs Call` works. An agent will
naturally reach for the dotted path. Consider stripping to the last component
or matching the qualified form.

## P3 — Empty symbol silently succeeds
`pyq defs ""` → "0 defs of ``", exit 0. Probably should be a usage error.

---

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

---

## P1 — syntactic `refs`/`callers` silently miss ALL attribute-access call sites
The syntactic engine matches only bare `Name` nodes, never `Attribute` access
(`obj.method()`). For `save` — a Django model method called everywhere:

```
ty        callers save  → 62
syntactic callers save  → 0
syntactic refs   save   → 0
grep '\.save('          → 469 call sites
```

`--syntactic` is advertised as the fast "grep-replacement / fallback," but for
the most common Python pattern (method calls on an object) it returns **0**, the
most dangerous possible answer: an agent reads "0 callers" as "dead, safe to
delete." Worse than grep, not a degraded-grep. Either match attribute accesses
syntactically (over-approximate, like grep) or make the fallback refuse
attribute-style queries loudly instead of answering 0.

Note the *good* side: ty's 62 vs grep's 469 is correct precision — ty resolves
which `save` (the in-repo model overrides) and doesn't conflate unrelated
`.save()` on other types. That disambiguation is the real value; don't lose it.

## P2 — "half-edited file still answers" is false; one parse error zeros the whole file
DESIGN.md: *"Parse errors non-fatal (an agent mid-edit still gets answers)"*;
README: *"a half-edited file still answers."* Actual behavior — a file with a
trailing syntax error yields **0** results for the entire file, including valid
defs/inputs *above* the error:

```python
import os
def alpha():           # valid
    return 1
KEY = os.environ["EARLY_KEY"]   # valid env read
class Broken(          # <- syntax error on last line
```
```
defs alpha   → 0      (should recover; it's a complete def before the error)
inputs       → 0      (should recover EARLY_KEY)
```

Resilience is only *file-granular*: a broken file does NOT break sibling clean
files (verified — those still answer). But the broken file itself returns
nothing. The "agent mid-edit" scenario — the stated motivation — is exactly the
failing case. `ruff_python_parser` does error-recovery and returns a best-effort
AST alongside the errors; pyq appears to discard the file when `errors` is
non-empty instead of walking the recovered tree. Walk the partial AST so
pre-error statements are still indexed.

**Root cause (confirmed in source):** `crates/pyq-index/src/extract.rs:26` —
`if let Ok(parsed) = parsed { ... }`. `parse_module` returns `Err` on any syntax
error while still carrying a best-effort recovered tree; the `if let Ok` throws
that tree away, so a single error skips the whole file. The doc-comment four
lines above (extract.rs:3-4) asserts the opposite ("Parse errors are non-fatal:
we extract what parsed and move on"). Fix: take the `Parsed` even in the `Err`
arm (e.g. `parse_module(...).map_or_else(|e| e.parsed?, identity)` / use the
unchecked accessor) and walk `parsed.syntax().body` regardless.

## P2 — `imports --cycles` false-positives on TYPE_CHECKING / deferred imports
The cycle detector counts `if TYPE_CHECKING:` imports (never execute at runtime)
and function-local/deferred imports (lazy, by design) as load-time cycle edges.
Reported cycle:

```
sara/utils.py:1:1  cycle: sara.utils ↔ sara.models ↔ call_log.models
```

Tracing the edges:
- `sara/utils.py:12  from sara.models import Appointment` — under `if
  TYPE_CHECKING:` (sara/utils.py:11). Never runs at runtime.
- `call_log/models.py:18  from sara.models import Appointment` — function-local
  (indented, deferred).

Both are exactly the patterns devs use to *break* runtime import cycles. pyq
flags the cycles that good code has already defused — a false positive that would
push an agent to "fix" non-problems. An import cycle that matters is a
*module-load-time* cycle; TYPE_CHECKING edges should be excluded outright (or
tagged `type-only`), and deferred/local imports separated from top-level ones.

Secondary: the `↔` notation implies each adjacent pair mutually imports, but
these are *directed* edges around a cycle (and the 16-module case is really a
strongly-connected component, not a single cycle). Use `→` and, ideally, report
the minimal cycle / the edge to cut, since that's the actionable output.

## P2 — `imports <module>` can't distinguish "not found" from "found, no edges"
A typo'd module and a real leaf module are indistinguishable, even in JSON:

```
imports scoring.modelz --reverse  → 0 importers of `scoring.modelz`   (typo)
imports scoring.apps   --reverse  → 0 importers of `scoring.apps`     (real, unused)
{"tool":"pyq","query":{"kind":"imports","mode":"reverse"},"summary":"0 importers …","count":0,"results":[]}
```

`--reverse` is sold as blast-radius ("who imports this"); "0 importers" reads as
"safe to delete." A typo silently produces that safe-looking answer. Need a
`module_found`/`resolved` signal (or a non-zero exit / error) when the queried
module isn't in the graph. (Minor: the JSON `query` block omits the queried
module name that `refs`/`defs` echo as `symbol` — add it for parity.)

## P1 — `refs` misses alias call sites that `callers` finds (verbs disagree)
With an aliased import `from pkg.core import make_widget as mw` and two `mw()`
calls:

```
callers make_widget  → 2   (app.py:2 mw(), app.py:3 mw() — alias resolved ✓)
refs    make_widget  → 3   (app.py:1 import binding, pkg/__init__.py re-export,
                            pkg/core.py def — NONE of the actual call sites)
```

`callers` resolves the alias back to the origin and finds the real uses; `refs`
lists only the alias's *binding* line and misses every *call* through it. So for
an aliased symbol the two verbs are nearly disjoint on usage, and `callers ⊄
refs` — even though every call is a reference and `refs` is documented as "every
reference (reads and calls)." An agent running `refs X` to see everything
touching `X` sees the import line but concludes it's barely used when it's called
twice; only `callers` reveals the truth. Either `refs` should follow the alias to
its use sites (preferred — match `callers`), or the verbs' alias semantics need
reconciling and documenting.

(Re-export through `__init__.py` and `import as` aliasing otherwise work well —
see Confirmed working. `callers` is the verb that gets aliases right.)

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
- `imports` honors `--root` (it's syntactic): `--root billing` → 224 edges,
  scoped; full repo → 4143. Internal-vs-`(ext)` classification looks right.
- `imports` relative-import resolution is correct when targets exist: `from .`,
  `from .pkg`, and `from .. import x` all map to the right internal module, and
  reverse-deps attribute them correctly. (Feeding invalid Python — a relative
  import past the top-level package — yields a junk `(ext)` target, but that's
  invalid input, not a bug.)
- `imports --cycles` and full-graph build are fast (~0.1-0.25s on ~1900 files).

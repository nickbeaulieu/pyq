"""The effect ledger: collect audited effects, attribute them to project FQNs,
and emit pyq's standard JSON envelope.

Attribution walks the live Python stack from the audit-event site to the nearest
frame inside the project (audit events fire deep in stdlib — `open` reports from
`io`, not from the caller). That nearest-project frame is the owner the static
`effects` verb would also attribute to, so the two join.
"""
from __future__ import annotations

import sys
from typing import Optional

from . import effects
from .fqn import frame_fqn

# How many stack frames sit between the audit hook callback and the code that
# triggered the event: hook -> sys.audit machinery. We start the walk above the
# hook frame itself.
_HOOK_FRAME_DEPTH = 2

_SOURCE_SUFFIXES = (".py", ".pyc", ".pyo", ".pyi")


def _is_loader_fs_noise(category: str, args: tuple) -> bool:
    """True for an fs event the import machinery performs on itself — not a
    user-code side effect.

    Bytecode loading is a *cluster* of fs events, not just an open: read the
    source (`open` `.py`), check/read the cache (`open` `.pyc`), then write it
    atomically (`os.mkdir __pycache__`, `open` a temp file, `os.rename` into
    place). The temp file carries a random suffix, so a `.pyc` check alone
    misses it. The unifying signal: any path argument that is a Python
    source/bytecode file or lives under `__pycache__`. A genuine
    `open("config.ini")` has neither and survives."""
    if category != "fs":
        return False
    # `open(fd, ...)` on an integer descriptor wraps an already-open fd; the
    # descriptor's creation (os.open/mkstemp) is the real fs access and is
    # audited separately, so the wrap is not a distinct effect. (The loader's
    # atomic bytecode write hits this path with an int fd.)
    if args and isinstance(args[0], int):
        return True
    for arg in args:
        if not isinstance(arg, str):
            continue
        if arg.endswith(_SOURCE_SUFFIXES):
            return True
        if "__pycache__" in arg.replace("\\", "/").split("/"):
            return True
    return False


class EffectLedger:
    """Accumulates deduped (category, event, owner) effect observations."""

    def __init__(self, root: str) -> None:
        self.root = root
        # (category, event, owner_fqn) -> hit count
        self._effects: dict[tuple[str, str, str], int] = {}
        # owners observed performing >=1 effect, for quick membership tests
        self.owners: set[str] = set()
        self._non_project_hits = 0

    def install(self) -> int:
        """Register the audit hook. Returns nothing useful; hooks can't be
        removed, so this is one-way for the life of the interpreter (fine for a
        one-shot trace process)."""
        sys.addaudithook(self._hook)
        return 0

    def _hook(self, event: str, args: tuple) -> None:
        category = effects.categorize(event)
        if category is None:
            return
        if _is_loader_fs_noise(category, args):
            # Import-machinery bookkeeping (reading source, writing bytecode),
            # not a side effect of user code. A genuine import-time
            # `open("config.ini")` touches no source/__pycache__ path and
            # survives this filter.
            return
        owner = self._nearest_project_owner()
        if owner is None:
            self._non_project_hits += 1
            return
        key = (category, event, owner)
        self._effects[key] = self._effects.get(key, 0) + 1
        self.owners.add(owner)

    def _nearest_project_owner(self) -> Optional[str]:
        frame = sys._getframe(_HOOK_FRAME_DEPTH)
        while frame is not None:
            code = frame.f_code
            fq = frame_fqn(code.co_filename, code.co_qualname, self.root)
            if fq is not None:
                return fq
            frame = frame.f_back
        return None

    # -- rendering -------------------------------------------------------
    def results(self) -> list[dict]:
        rows = [
            {
                "effect": category,
                "event": event,
                "owner": owner,
                "hits": hits,
            }
            for (category, event, owner), hits in self._effects.items()
        ]
        rows.sort(key=lambda r: (r["owner"], r["effect"], r["event"]))
        return rows

    def envelope(self, query: dict) -> dict:
        rows = self.results()
        warnings = [
            "Observed effects only — a category with no rows means it was not "
            "audited this run, not that the code is free of it.",
            "Audit hook cannot see env-reads, random, or clock effects; use the "
            "static `effects`/`inputs` verbs for those (categories: "
            + ", ".join(effects.UNAUDITED_CATEGORIES)
            + ").",
        ]
        if self._non_project_hits:
            warnings.append(
                f"{self._non_project_hits} audited effect(s) had no project "
                "frame on the stack (pure third-party/stdlib activity) and were "
                "dropped."
            )
        return {
            "tool": "pyq",
            "query": query,
            "summary": f"{len(rows)} observed effect(s) across "
            f"{len(self.owners)} callable(s)",
            "count": len(rows),
            "results": rows,
            "warnings": warnings,
        }

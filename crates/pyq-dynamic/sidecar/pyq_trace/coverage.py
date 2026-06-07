"""Per-test line coverage via `sys.monitoring` (CPython 3.12+).

The change-coverage seam (TASKS.md #9.4): record which project lines actually
execute, attributed to the test that ran them, so a diff can be told which
changed lines a test exercises — the runtime oracle behind the `tests` verb's
"a static 0 is not 'untested'" caveat, and the trimmer for `deadcode`'s
over-approximation.

`sys.monitoring` is 3.12+. On older interpreters the tracker reports itself
unavailable and the driver degrades (reports changed lines with unknown
coverage) rather than crashing — the audit-hook effect ledger still works there.

Unlike the effect ledger's PY_START (first-touch, then `DISABLE`), coverage
needs *every* line event, so the LINE callback never disables. The per-file
relpath is cached so the hot path is a dict lookup, not a syscall.
"""
from __future__ import annotations

import sys
from typing import Optional

from .fqn import relpath_under_root

_TOOL_ID = 2  # distinct from coverage.py's reserved id (1) and any debugger (0)


class CoverageTracker:
    def __init__(self, root: str) -> None:
        self.root = root
        self.available = sys.version_info >= (3, 12) and hasattr(sys, "monitoring")
        # nodeid -> set[(relpath, line)]
        self.per_test: dict[str, set] = {}
        # union of all executed (relpath, line)
        self.all_lines: set = set()
        self._current: Optional[set] = None
        self._rel_cache: dict[str, Optional[str]] = {}

    def install(self) -> bool:
        if not self.available:
            return False
        mon = sys.monitoring
        mon.use_tool_id(_TOOL_ID, "pyq-coverage")
        mon.register_callback(_TOOL_ID, mon.events.LINE, self._on_line)
        mon.set_events(_TOOL_ID, mon.events.LINE)
        return True

    def uninstall(self) -> None:
        if not self.available:
            return
        mon = sys.monitoring
        mon.set_events(_TOOL_ID, 0)
        mon.free_tool_id(_TOOL_ID)

    def start_test(self, nodeid: str) -> None:
        self._current = self.per_test.setdefault(nodeid, set())

    def stop_test(self) -> None:
        self._current = None

    def _rel(self, co_filename: str) -> Optional[str]:
        rel = self._rel_cache.get(co_filename, False)
        if rel is False:
            rel = relpath_under_root(co_filename, self.root)
            self._rel_cache[co_filename] = rel
        return rel

    def _on_line(self, code, line_number):
        rel = self._rel(code.co_filename)
        if rel is None:
            return
        key = (rel, line_number)
        self.all_lines.add(key)
        if self._current is not None:
            self._current.add(key)

    def to_dict(self) -> dict:
        """Serializable coverage: per-test (file, line) pairs + the file→lines
        union, plus enough provenance for the driver to degrade honestly."""
        files: dict[str, list] = {}
        for rel, line in sorted(self.all_lines):
            files.setdefault(rel, []).append(line)
        tests = {
            nodeid: sorted([list(k) for k in keys])
            for nodeid, keys in sorted(self.per_test.items())
        }
        return {
            "python": ".".join(str(v) for v in sys.version_info[:3]),
            "monitoring_available": self.available,
            "files": files,
            "tests": tests,
        }

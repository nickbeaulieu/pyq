"""Observed return-type shapes via `sys.monitoring` (CPython 3.12+).

The first slice of the protocol/structural-conformance surface (TASKS.md #9.5 /
#21): record the concrete return type each project callable actually produced at
runtime. Static type inference is ty's job; this is the complementary runtime
*evidence* — "what did this function really return across the suite" — useful
for spotting missing/loose annotations and narrowing `Protocol`s.

Return values are captured at `PY_RETURN`, which hands the callback the value
directly (no frame walking). Argument-type capture is deliberately out of scope
for this first cut — it needs frame-local introspection with murkier
reliability, and return types alone are the high-signal half.

3.12+ only; on older interpreters the tracker reports itself unavailable.
"""
from __future__ import annotations

import sys
from typing import Optional

from .fqn import frame_fqn

_TOOL_ID = 4  # distinct from coverage (2) / reserved debugger (0) / coverage.py (1)


def type_label(value: object) -> str:
    """A readable type id: bare name for builtins (`int`, `NoneType`), else
    `module.Qualname` so a project type joins back to its static FQN."""
    t = type(value)
    name = getattr(t, "__qualname__", t.__name__)
    mod = getattr(t, "__module__", "")
    return name if mod in ("builtins", "") else f"{mod}.{name}"


class ShapeTracker:
    def __init__(self, root: str) -> None:
        self.root = root
        self.available = sys.version_info >= (3, 12) and hasattr(sys, "monitoring")
        # fqn -> set[type_label]
        self.returns: dict[str, set] = {}

    def install(self) -> bool:
        if not self.available:
            return False
        mon = sys.monitoring
        mon.use_tool_id(_TOOL_ID, "pyq-shapes")
        mon.register_callback(_TOOL_ID, mon.events.PY_RETURN, self._on_return)
        mon.set_events(_TOOL_ID, mon.events.PY_RETURN)
        return True

    def uninstall(self) -> None:
        if not self.available:
            return
        mon = sys.monitoring
        mon.set_events(_TOOL_ID, 0)
        mon.free_tool_id(_TOOL_ID)

    def _on_return(self, code, instruction_offset, retval):
        # A module body "returns" None on completion — an artifact, not a
        # callable's return shape. Skip module-scope frames.
        if code.co_qualname == "<module>":
            return
        fqn: Optional[str] = frame_fqn(code.co_filename, code.co_qualname, self.root)
        if fqn is None:
            return
        self.returns.setdefault(fqn, set()).add(type_label(retval))

    def to_dict(self) -> dict:
        return {
            "python": ".".join(str(v) for v in sys.version_info[:3]),
            "monitoring_available": self.available,
            "returns": {
                fqn: sorted(labels) for fqn, labels in sorted(self.returns.items())
            },
        }

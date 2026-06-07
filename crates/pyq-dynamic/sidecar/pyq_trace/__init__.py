"""pyq dynamic-tier sidecar (Phase 1: audit-hook effect ledger).

A bundled Python package the pyq CLI runs inside the target interpreter. It
installs `sys.addaudithook` to record the side effects the code *actually*
performs at runtime, keyed by the same fully-qualified ids the static index
uses, so the two tiers join (effect-diff). Standalone-runnable for development:

    python -m pyq_trace --root <project> --run <module-or-script> [args...]

emits the standard pyq JSON envelope on stdout.
"""
from __future__ import annotations

from .ledger import EffectLedger

__all__ = ["EffectLedger", "trace"]


def trace(root: str) -> EffectLedger:
    """Install the effect ledger for `root` and return it. Call once, early,
    before importing/running the code under trace."""
    ledger = EffectLedger(root)
    ledger.install()
    return ledger

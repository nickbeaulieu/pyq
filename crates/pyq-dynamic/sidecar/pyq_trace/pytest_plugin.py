"""pytest plugin: run the suite under the effect ledger.

The pytest-first driver (TASKS.md #9.2). Registered by the Rust `pyq-dynamic`
crate with `-p pyq_trace.pytest_plugin`; configured via two env vars so no
pytest CLI surface leaks into the target's own argv:

    PYQ_TRACE_ROOT   project scan root (defaults to pytest's rootdir)
    PYQ_TRACE_OUT    where to write the envelope (defaults to stderr)

The hook is installed in `pytest_configure`, which fires before collection — so
the import-time effects of the target modules (imported as pytest collects the
test files) are captured, not just per-test runtime effects.
"""
from __future__ import annotations

import json
import os
import sys

from . import trace

_ledger = None


def pytest_configure(config):
    global _ledger
    root = os.environ.get("PYQ_TRACE_ROOT") or str(config.rootdir)
    _ledger = trace(os.path.abspath(root))


def pytest_unconfigure(config):
    if _ledger is None:
        return
    query = {
        "kind": "effects-observed",
        "root": _ledger.root,
        "driver": "pytest",
    }
    envelope = _ledger.envelope(query)
    out = os.environ.get("PYQ_TRACE_OUT")
    if out:
        with open(out, "w", encoding="utf-8") as fh:
            json.dump(envelope, fh)
    else:
        sys.stderr.write("\n" + json.dumps(envelope) + "\n")

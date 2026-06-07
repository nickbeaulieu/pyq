"""pytest plugin: run the suite under the effect ledger.

The pytest-first driver (TASKS.md #9.2). Registered by the Rust `pyq-dynamic`
crate with `-p pyq_trace.pytest_plugin`; configured via two env vars so no
pytest CLI surface leaks into the target's own argv:

    PYQ_TRACE_ROOT   project scan root (defaults to pytest's rootdir)
    PYQ_TRACE_OUT    where to write the effect envelope (defaults to stderr)
    PYQ_COV_OUT      if set, also collect per-test line coverage here (3.12+)

The effect hook is installed in `pytest_configure`, which fires before
collection — so the import-time effects of the target modules (imported as
pytest collects the test files) are captured, not just per-test runtime effects.
Coverage (when requested) is scoped per test via the runtest log hooks.
"""
from __future__ import annotations

import json
import os
import sys

from . import trace
from .coverage import CoverageTracker
from .shapes import ShapeTracker

_ledger = None
_cov = None
_shapes = None


def pytest_configure(config):
    global _ledger, _cov, _shapes
    root = os.path.abspath(os.environ.get("PYQ_TRACE_ROOT") or str(config.rootdir))
    _ledger = trace(root)
    if os.environ.get("PYQ_COV_OUT"):
        _cov = CoverageTracker(root)
        _cov.install()
    if os.environ.get("PYQ_SHAPES_OUT"):
        _shapes = ShapeTracker(root)
        _shapes.install()


def pytest_runtest_logstart(nodeid, location):
    if _cov is not None:
        _cov.start_test(nodeid)


def pytest_runtest_logfinish(nodeid, location):
    if _cov is not None:
        _cov.stop_test()


def pytest_unconfigure(config):
    if _ledger is not None:
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

    if _cov is not None:
        _cov.uninstall()
        cov_out = os.environ.get("PYQ_COV_OUT")
        if cov_out:
            with open(cov_out, "w", encoding="utf-8") as fh:
                json.dump(_cov.to_dict(), fh)

    if _shapes is not None:
        _shapes.uninstall()
        shapes_out = os.environ.get("PYQ_SHAPES_OUT")
        if shapes_out:
            with open(shapes_out, "w", encoding="utf-8") as fh:
                json.dump(_shapes.to_dict(), fh)

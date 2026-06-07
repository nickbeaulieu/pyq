"""CoverageTracker + the relpath helper it keys on."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from pyq_trace.coverage import CoverageTracker  # noqa: E402
from pyq_trace.fqn import relpath_under_root  # noqa: E402

ROOT = "/proj"


def test_relpath_under_root():
    assert relpath_under_root("/proj/pkg/models.py", ROOT) == "pkg/models.py"
    assert relpath_under_root("/usr/lib/python3.12/socket.py", ROOT) is None
    assert relpath_under_root("<string>", ROOT) is None


def test_tracker_records_per_test_lines():
    # Drive the callback directly (no interpreter hooks) to test bookkeeping.
    t = CoverageTracker(ROOT)

    class FakeCode:
        co_filename = "/proj/pkg/models.py"

    t.start_test("tests/test_x.py::test_a")
    t._on_line(FakeCode(), 5)
    t._on_line(FakeCode(), 6)
    t.stop_test()
    # a line executed outside any test still lands in the union, not per-test
    t._on_line(FakeCode(), 99)

    d = t.to_dict()
    assert d["files"]["pkg/models.py"] == [5, 6, 99]
    assert d["tests"]["tests/test_x.py::test_a"] == [["pkg/models.py", 5], ["pkg/models.py", 6]]


def test_tracker_ignores_non_project_files():
    t = CoverageTracker(ROOT)

    class Outside:
        co_filename = "/usr/lib/python3.12/json/__init__.py"

    t.start_test("t::x")
    t._on_line(Outside(), 1)
    t.stop_test()
    assert t.to_dict()["files"] == {}


def test_availability_matches_interpreter():
    t = CoverageTracker(ROOT)
    expected = sys.version_info >= (3, 12) and hasattr(sys, "monitoring")
    assert t.available is expected

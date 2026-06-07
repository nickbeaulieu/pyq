"""ShapeTracker return-type observation + type labelling."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from pyq_trace.shapes import ShapeTracker, type_label  # noqa: E402

ROOT = "/proj"


def test_type_label_builtins_are_bare():
    assert type_label(1) == "int"
    assert type_label("x") == "str"
    assert type_label(None) == "NoneType"
    assert type_label([1]) == "list"


def test_type_label_project_types_are_qualified():
    class Widget:  # __module__ is this test module, not builtins
        pass

    label = type_label(Widget())
    assert label.endswith(".Widget")
    assert "builtins" not in label


def test_tracker_unions_return_types_per_callable():
    t = ShapeTracker(ROOT)

    class Code:
        co_filename = "/proj/pkg/calc.py"
        co_qualname = "add"

    t._on_return(Code(), 0, 3)       # int
    t._on_return(Code(), 0, 3.5)     # float
    t._on_return(Code(), 0, 1)       # int again -> deduped

    d = t.to_dict()
    assert d["returns"]["pkg.calc.add"] == ["float", "int"]


def test_module_scope_returns_are_skipped():
    t = ShapeTracker(ROOT)

    class ModCode:
        co_filename = "/proj/pkg/calc.py"
        co_qualname = "<module>"

    t._on_return(ModCode(), 0, None)
    assert t.to_dict()["returns"] == {}

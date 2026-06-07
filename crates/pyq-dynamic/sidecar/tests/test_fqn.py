"""FQN join — the make-or-break contract with the static index."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from pyq_trace.fqn import class_node_of, frame_fqn, module_components  # noqa: E402

ROOT = "/proj"


def test_module_components_drops_init_and_suffix():
    assert module_components("pkg/models.py") == ["pkg", "models"]
    assert module_components("pkg/__init__.py") == ["pkg"]
    assert module_components("app.py") == ["app"]


def test_basic_function_fqn():
    assert frame_fqn("/proj/pkg/models.py", "make_user", ROOT) == "pkg.models.make_user"


def test_method_fqn():
    assert frame_fqn("/proj/pkg/models.py", "User.greet", ROOT) == "pkg.models.User.greet"


def test_locals_segments_are_stripped():
    # CPython injects <locals>; pyq's scope path does not. Must normalize.
    assert (
        frame_fqn("/proj/pkg/models.py", "outer.<locals>.inner", ROOT)
        == "pkg.models.outer.inner"
    )


def test_module_scope_frame_is_the_module():
    assert frame_fqn("/proj/app.py", "<module>", ROOT) == "app"


def test_comprehension_and_lambda_scopes_collapse_to_enclosing():
    # An effect inside a comprehension/lambda should attribute to its real
    # enclosing scope, not to the synthetic `<listcomp>`/`<lambda>` segment.
    assert frame_fqn("/proj/app.py", "build.<locals>.<listcomp>", ROOT) == "app.build"
    assert frame_fqn("/proj/app.py", "f.<lambda>", ROOT) == "app.f"


def test_outside_root_is_none():
    assert frame_fqn("/usr/lib/python3.12/socket.py", "socket.connect", ROOT) is None


def test_synthetic_code_object_is_none():
    assert frame_fqn("<string>", "<module>", ROOT) is None


def test_symlinked_root_still_joins(tmp_path):
    # The import system resolves co_filename through symlinks (macOS /tmp ->
    # /private/tmp); a raw --root may be the unresolved path. Both ends must be
    # canonicalized or every effect is dropped as "outside root".
    real = tmp_path / "real"
    (real / "pkg").mkdir(parents=True)
    src = real / "pkg" / "models.py"
    src.write_text("x = 1\n")
    link = tmp_path / "link"
    os.symlink(real, link)
    # frame reports the resolved real path; root is given as the symlink.
    assert frame_fqn(str(src.resolve()), "make_user", str(link)) == "pkg.models.make_user"


def test_class_node_folds_init_onto_class():
    assert class_node_of("pkg.models.User.__init__") == "pkg.models.User"
    assert class_node_of("pkg.models.make_user") == "pkg.models.make_user"

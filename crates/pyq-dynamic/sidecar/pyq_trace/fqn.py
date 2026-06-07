"""Runtime-frame -> pyq static FQN join.

The dynamic tier's facts are only useful if they key on the *same* identifiers
the static index uses (`pkg.models.User.__init__`). pyq builds those in
`crates/pyq-resolve/src/graph.rs::scope_fqn` as
`module_components(relpath).join(".") + scope`. This module reconstructs that
exact string from a runtime code object, so a dynamic observation maps straight
onto a static node.

Two normalizations the Phase-0 spike pinned down:
  1. CPython's `co_qualname` injects `<locals>` for functions nested in a
     function (`outer.<locals>.inner`); pyq's scope path is just the enclosing
     names (`outer.inner`). We strip `<locals>` segments.
  2. Instantiation runs `Cls.__init__`, but pyq's call graph node for a
     constructor edge is the *class* `Cls`, not `Cls.__init__`. Callers that
     join against call-graph nodes should fold `X.__init__` -> `X` via
     `class_node_of`; the ledger itself records the faithful `X.__init__`.
"""
from __future__ import annotations

import os
from functools import lru_cache
from typing import Optional


@lru_cache(maxsize=None)
def _real(path: str) -> str:
    """realpath, memoized. The set of distinct code-object filenames and the
    single root are small and bounded, so this stays cheap while making the
    join robust to symlinked paths (e.g. macOS `/tmp` -> `/private/tmp`, which
    the import system resolves but a raw `--root` argument does not)."""
    return os.path.realpath(path)


def module_components(relpath: str) -> list[str]:
    """Path relative to the scan root -> dotted module components.

    Mirrors `module_components` in crates/pyq-resolve/src/unified.rs: drop the
    `.py`/`.pyi` suffix, split on the path separator, and drop empty and
    `__init__` components.
    """
    stem = relpath
    for suffix in (".pyi", ".py"):
        if stem.endswith(suffix):
            stem = stem[: -len(suffix)]
            break
    parts = stem.replace("\\", "/").split("/")
    return [p for p in parts if p and p != "__init__"]


def frame_fqn(co_filename: str, co_qualname: str, root: str) -> Optional[str]:
    """The pyq FQN for a runtime code object, or None if outside the project.

    `root` is the scan root (the directory pyq was pointed at). Code whose file
    lives outside `root` (stdlib, site-packages) returns None — it has no static
    node to join against.
    """
    if not _looks_like_source(co_filename):
        return None
    try:
        rel = os.path.relpath(_real(co_filename), _real(root))
    except ValueError:
        # different drive on Windows
        return None
    if rel.startswith(".."):
        return None
    # Drop every synthetic, bracketed scope segment CPython injects into
    # co_qualname: `<locals>` (function nested in a function), `<module>`
    # (module-scope code), and comprehension/lambda scopes (`<listcomp>`,
    # `<lambda>`, ...). What remains is the chain of real named scopes — the
    # owner the static index attributes to (`outer.<locals>.inner` -> `outer.inner`,
    # `<module>` -> module, `f.<listcomp>` -> `f`).
    qual = ".".join(
        p for p in co_qualname.split(".") if not (p.startswith("<") and p.endswith(">"))
    )
    module = ".".join(module_components(rel))
    if not qual:
        return module or None
    return f"{module}.{qual}" if module else qual


def relpath_under_root(co_filename: str, root: str) -> Optional[str]:
    """The project-relative path of a code object's file (`pkg/models.py`), or
    None if it lives outside `root`. Symlink-robust, like `frame_fqn`. Used by
    line coverage, which keys on (file, line) rather than FQN."""
    if not _looks_like_source(co_filename):
        return None
    try:
        rel = os.path.relpath(_real(co_filename), _real(root))
    except ValueError:
        return None
    if rel.startswith(".."):
        return None
    return rel.replace("\\", "/")


def class_node_of(fqn: str) -> str:
    """Fold an observed constructor FQN onto its call-graph node.

    `pkg.models.User.__init__` -> `pkg.models.User` (the class node a static
    instantiation edge points at). Leaves every other FQN untouched.
    """
    suffix = ".__init__"
    return fqn[: -len(suffix)] if fqn.endswith(suffix) else fqn


def _looks_like_source(co_filename: str) -> bool:
    # Synthetic code objects (`<string>`, `<stdin>`, frozen importlib) have no
    # source file and therefore no static node.
    return not (co_filename.startswith("<") and co_filename.endswith(">"))

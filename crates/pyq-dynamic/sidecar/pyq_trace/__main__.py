"""Standalone entry point for the Phase-1 effect-ledger sidecar.

    python -m pyq_trace --root <project> --run <module> [args...]      # run a module
    python -m pyq_trace --root <project> --script <path.py> [args...]  # run a script

The hook is installed *before* the target is imported/executed so import-time
effects are captured. Phase 2's Rust driver will instead install the ledger from
a pytest plugin and run the suite; this CLI is the development harness and the
shape Phase 2 wires to.
"""
from __future__ import annotations

import argparse
import json
import os
import runpy
import sys

from . import trace


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="pyq_trace", add_help=True)
    parser.add_argument("--root", required=True, help="project scan root")
    parser.add_argument("--pretty", action="store_true", help="indent the JSON")
    parser.add_argument(
        "--out",
        metavar="PATH",
        help="write the envelope here instead of stdout (keeps the target's "
        "own stdout/stderr uncontaminated — required for machine consumption)",
    )
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--run", metavar="MODULE", help="run a module as __main__")
    group.add_argument("--script", metavar="PATH", help="run a .py file as __main__")
    parser.add_argument("rest", nargs=argparse.REMAINDER, help="argv for the target")

    args = parser.parse_args(argv)
    root = os.path.abspath(args.root)

    ledger = trace(root)

    # Hand the target a clean argv, with sys.path[0] pointing at the root so its
    # first-party imports resolve the way pytest/`python` would.
    target = args.run or args.script
    sys.argv = [target, *args.rest]
    if root not in sys.path:
        sys.path.insert(0, root)

    exit_code = 0
    try:
        if args.run:
            runpy.run_module(args.run, run_name="__main__", alter_sys=True)
        else:
            runpy.run_path(args.script, run_name="__main__")
    except SystemExit as exc:
        exit_code = exc.code if isinstance(exc.code, int) else 1
    except BaseException as exc:  # noqa: BLE001 - report, don't crash the trace
        exit_code = 1
        print(f"pyq_trace: target raised {type(exc).__name__}: {exc}",
              file=sys.stderr)

    query = {
        "kind": "effects-observed",
        "root": root,
        "target": target,
        "exit_code": exit_code,
    }
    envelope = ledger.envelope(query)
    indent = 2 if args.pretty else None
    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump(envelope, fh, indent=indent)
            fh.write("\n")
    else:
        json.dump(envelope, sys.stdout, indent=indent)
        sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
